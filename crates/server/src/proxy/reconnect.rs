use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::daemon::handshake::RefusalKind;
use crate::daemon::spawn::{AttemptError, Budgets, Connected, DaemonConn, RoundError, run_round};
use crate::proxy::track::{KeptRequest, Link, Outcome, Tracker, request_id, response_outcome};
use crate::proxy::{
    SharedTracker, StdoutTx, connect, jsonrpc_error, lock, resolve_attempt, send_json,
};

pub const RESTART_CODE: i64 = -32603;
pub const RESTART_MESSAGE: &str = "ferrex daemon restarted, retry";

const MCP_HINT: &str = "run /mcp reconnect so Claude starts the new ferrex binary";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossAction {
    WaitForRequest,
    /// Startup is still under way and the client is waiting on its first request.
    ReconnectNow(AfterHello),
}

/// What an attempt does on the new connection once the hello has succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterHello {
    /// Re-sends the cached `initialize` untracked and discards its response.
    ReplayDrop,
    /// Re-sends the kept `initialize`, still in flight, and returns its response for the client.
    ReplayForward,
    /// Re-sends the kept first request; its response arrives through normal forwarding.
    ResendKept,
    /// The caller flushes the queue straight after the hello.
    Nothing,
}

/// The re-sent inline first request whose response is still outstanding; its round stays open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResendPending {
    generation: u64,
    attempts_used: u8,
}

impl ResendPending {
    #[must_use]
    pub const fn new(kept: &KeptRequest, attempts_used: u8) -> Self {
        Self {
            generation: kept.generation,
            attempts_used,
        }
    }

    /// The attempt count only carries over while this re-send is still the kept in-flight request.
    #[must_use]
    pub fn attempts_carried(&self, tracker: &Tracker) -> u8 {
        tracker
            .kept_in_flight()
            .filter(|kept| kept.generation == self.generation)
            .map_or(0, |_| self.attempts_used)
    }
}

#[derive(Debug)]
pub struct ReconnectCtx {
    pub exe: PathBuf,
    pub argv: Vec<OsString>,
    pub budgets: Budgets,
    pub startup_mtime: Option<SystemTime>,
    pub last_key: String,
    pub tracker: SharedTracker,
}

impl ReconnectCtx {
    #[must_use]
    pub fn failure_message(&self, err: &RoundError) -> String {
        let message = format!("ferrex daemon unavailable: {}", err.last);
        if needs_mcp_hint(
            &err.last,
            self.exe.exists(),
            err.any_child_exited_early,
            exe_mtime(&self.exe),
            self.startup_mtime,
        ) {
            format!("{message}; {MCP_HINT}")
        } else {
            message
        }
    }
}

#[must_use]
pub fn exe_mtime(exe: &Path) -> Option<SystemTime> {
    std::fs::metadata(exe).and_then(|m| m.modified()).ok()
}

/// The new binary can't serve this proxy, so only a fresh proxy (`/mcp` reconnect) helps.
fn needs_mcp_hint(
    last: &AttemptError,
    exe_exists: bool,
    any_child_exited_early: bool,
    mtime_now: Option<SystemTime>,
    startup_mtime: Option<SystemTime>,
) -> bool {
    last.kind == Some(RefusalKind::Protocol)
        || !exe_exists
        || (any_child_exited_early && mtime_now != startup_mtime)
}

/// Answers the in-flight requests the daemon will never answer with the restart error; before
/// init the kept first request is spared, since it gets re-sent at once.
pub fn on_daemon_loss(tracker: &SharedTracker, out: &StdoutTx) -> LossAction {
    let mut tracker = lock(tracker);
    let Some(kept) = tracker.kept_in_flight() else {
        answer(tracker.take_in_flight(), out, RESTART_MESSAGE);
        return LossAction::WaitForRequest;
    };
    let after = if kept.initialize {
        AfterHello::ReplayForward
    } else {
        AfterHello::ResendKept
    };
    let keep = kept.id.clone();
    answer(tracker.take_in_flight_except(&keep), out, RESTART_MESSAGE);
    LossAction::ReconnectNow(after)
}

/// Answers every in-flight request, queued ones included, with `message`; the link goes down.
pub fn fail_pending(tracker: &SharedTracker, out: &StdoutTx, message: &str) {
    let mut tracker = lock(tracker);
    answer(tracker.take_in_flight(), out, message);
    tracker.set_link(Link::Down);
}

fn answer(ids: impl IntoIterator<Item = Value>, out: &StdoutTx, message: &str) {
    for id in ids {
        send_json(out, jsonrpc_error(&id, RESTART_CODE, message));
    }
}

/// Runs a round of `connect_or_spawn`, hello and `after`, re-resolving the key each attempt.
pub async fn reconnect(
    ctx: &ReconnectCtx,
    after: AfterHello,
    used: u8,
) -> Result<Connected<(DaemonConn, Option<Vec<u8>>)>, RoundError> {
    let ReconnectCtx {
        exe,
        argv,
        budgets,
        tracker,
        ..
    } = ctx;
    run_round(
        || resolve_attempt(None, argv, exe),
        |resolved| async move {
            let mut conn = connect(resolved, exe, argv, budgets).await?;
            let response = match after {
                AfterHello::ReplayDrop => {
                    replay(&mut conn, tracker, budgets.replay).await?;
                    None
                }
                AfterHello::ReplayForward => {
                    Some(forward_initialize(&mut conn, tracker, budgets.replay).await?)
                }
                AfterHello::ResendKept => {
                    write_to_daemon(&mut conn.writer, &kept(tracker)?.line).await?;
                    None
                }
                AfterHello::Nothing => None,
            };
            Ok((conn, response))
        },
        used,
    )
    .await
}

/// Re-sends the cached handshake on a fresh connection, without tracking the `initialize` id.
pub async fn replay(
    conn: &mut DaemonConn,
    tracker: &SharedTracker,
    timeout: Duration,
) -> Result<(), AttemptError> {
    let (initialize, initialized) = {
        let mut tracker = lock(tracker);
        tracker.mark_replay_started();
        (
            tracker.cached_initialize().map(<[u8]>::to_vec),
            tracker.cached_initialized().map(<[u8]>::to_vec),
        )
    };
    let initialize =
        initialize.ok_or_else(|| AttemptError::fatal("no cached initialize to replay"))?;
    let id = request_id(&initialize)
        .ok_or_else(|| AttemptError::fatal("cached initialize has no id"))?;
    write_to_daemon(&mut conn.writer, &initialize).await?;
    let what = "replayed initialize";
    let (_, outcome) = await_response(&mut conn.reader, &id, timeout, what).await?;
    if outcome == Outcome::Error {
        return Err(AttemptError::retryable(
            "daemon answered the replayed initialize with an error",
        ));
    }
    if let Some(initialized) = initialized {
        write_to_daemon(&mut conn.writer, &initialized).await?;
    }
    Ok(())
}

/// Re-sends the kept `initialize`, whose id stays in flight, and returns its response line.
async fn forward_initialize(
    conn: &mut DaemonConn,
    tracker: &SharedTracker,
    timeout: Duration,
) -> Result<Vec<u8>, AttemptError> {
    let KeptRequest { id, line, .. } = kept(tracker)?;
    write_to_daemon(&mut conn.writer, &line).await?;
    let what = "re-sent initialize";
    let (response, _) = await_response(&mut conn.reader, &id, timeout, what).await?;
    Ok(response)
}

fn kept(tracker: &SharedTracker) -> Result<KeptRequest, AttemptError> {
    lock(tracker)
        .kept_first()
        .cloned()
        .ok_or_else(|| AttemptError::fatal("no kept first request to re-send"))
}

async fn write_to_daemon(writer: &mut OwnedWriteHalf, line: &[u8]) -> Result<(), AttemptError> {
    writer
        .write_all(line)
        .await
        .map_err(|e| AttemptError::retryable(format!("write to daemon failed: {e}")))
}

/// A timeout on the still-open connection ends the round; EOF is retryable.
async fn await_response(
    reader: &mut BufReader<OwnedReadHalf>,
    id: &Value,
    timeout: Duration,
    what: &str,
) -> Result<(Vec<u8>, Outcome), AttemptError> {
    tokio::time::timeout(timeout, read_response(reader, id, what))
        .await
        .map_err(|_| {
            AttemptError::fatal(format!(
                "daemon did not answer the {what} within {timeout:?}"
            ))
        })?
}

async fn read_response(
    reader: &mut BufReader<OwnedReadHalf>,
    id: &Value,
    what: &str,
) -> Result<(Vec<u8>, Outcome), AttemptError> {
    loop {
        let mut line = Vec::new();
        let n = reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|e| AttemptError::retryable(format!("read from daemon failed: {e}")))?;
        if n == 0 || !line.ends_with(b"\n") {
            return Err(AttemptError::retryable(format!(
                "daemon closed the connection before answering the {what}"
            )));
        }
        if let Some(outcome) = response_outcome(&line, id) {
            return Ok((line, outcome));
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use tokio::net::UnixStream;
    use tokio::sync::mpsc;

    use super::*;
    use crate::proxy::Out;
    use crate::proxy::track::ClientLine;

    fn failure(kind: Option<RefusalKind>) -> AttemptError {
        AttemptError {
            kind,
            retryable: true,
            child_exited_early: false,
            message: "boom".into(),
        }
    }

    #[test]
    fn mcp_hint_triggers() {
        let then = Some(SystemTime::UNIX_EPOCH);
        let now = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1));
        let protocol = failure(Some(RefusalKind::Protocol));
        let other = failure(Some(RefusalKind::Key));
        assert!(needs_mcp_hint(&protocol, true, false, then, then));
        assert!(needs_mcp_hint(&other, false, false, then, then));
        assert!(needs_mcp_hint(&other, true, true, now, then));
        assert!(needs_mcp_hint(&other, true, true, None, then));
    }

    #[test]
    fn mcp_hint_not_triggered() {
        let then = Some(SystemTime::UNIX_EPOCH);
        let now = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1));
        let other = failure(None);
        assert!(!needs_mcp_hint(&other, true, false, then, then));
        assert!(!needs_mcp_hint(&other, true, true, then, then));
        assert!(!needs_mcp_hint(&other, true, false, now, then));
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Out>) -> Vec<serde_json::Value> {
        let mut lines = Vec::new();
        while let Ok(Out::Line(line, _)) = rx.try_recv() {
            lines.push(serde_json::from_slice(&line).unwrap());
        }
        lines
    }

    #[test]
    fn resend_pending_carries_attempts_while_kept() {
        let call = b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\"}\n";
        let tracker = tracker_with(&[call], &[]);
        let pending = ResendPending::new(lock(&tracker).kept_first().unwrap(), 2);
        assert_eq!(pending.attempts_carried(&lock(&tracker)), 2);
        lock(&tracker).on_daemon_line(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\n");
        assert_eq!(pending.attempts_carried(&lock(&tracker)), 0);
    }

    #[test]
    fn reused_kept_id_starts_a_fresh_round() {
        let call = b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\"}\n";
        let tracker = tracker_with(&[call], &[]);
        let pending = ResendPending::new(lock(&tracker).kept_first().unwrap(), 2);
        {
            let mut t = lock(&tracker);
            t.on_daemon_line(
                b"{\"jsonrpc\":\"2.0\",\"id\":3,\"error\":{\"code\":1,\"message\":\"x\"}}\n",
            );
            t.on_client_line(call);
            assert!(t.kept_in_flight().is_some());
        }
        assert_eq!(pending.attempts_carried(&lock(&tracker)), 0);
    }

    fn restart_error(id: u64) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": RESTART_CODE, "message": RESTART_MESSAGE},
        })
    }

    const INITIALIZE: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n";
    const INITIALIZED: &[u8] = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
    const INIT_RESULT: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
    const INIT_ERROR: &[u8] =
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":1,\"message\":\"x\"}}\n";

    fn tracker_with(client: &[&[u8]], daemon: &[&[u8]]) -> SharedTracker {
        let tracker = SharedTracker::default();
        let mut t = lock(&tracker);
        for line in client {
            t.on_client_line(line);
        }
        for line in daemon {
            t.on_daemon_line(line);
        }
        drop(t);
        tracker
    }

    fn loss(tracker: &SharedTracker) -> (LossAction, Vec<serde_json::Value>) {
        let (out, mut rx) = mpsc::unbounded_channel();
        let action = on_daemon_loss(tracker, &out);
        (action, drain(&mut rx))
    }

    #[test]
    fn loss_after_init_answers_everything() {
        let tracker = tracker_with(
            &[
                INITIALIZE,
                b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\"}\n",
            ],
            &[INIT_RESULT],
        );
        assert_eq!(
            loss(&tracker),
            (LossAction::WaitForRequest, vec![restart_error(5)])
        );
        assert!(lock(&tracker).take_in_flight().is_empty());
    }

    #[test]
    fn loss_with_initialize_in_flight_forwards_it_again() {
        let ping = b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n";
        let tracker = tracker_with(&[ping, INITIALIZE], &[]);
        assert_eq!(
            loss(&tracker),
            (
                LossAction::ReconnectNow(AfterHello::ReplayForward),
                vec![restart_error(2)]
            )
        );
        assert_eq!(lock(&tracker).take_in_flight(), vec![serde_json::json!(1)]);
    }

    #[test]
    fn loss_with_inline_first_request_in_flight_resends_it() {
        let tracker = tracker_with(
            &[
                b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\"}\n",
                b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/list\"}\n",
            ],
            &[],
        );
        assert_eq!(
            loss(&tracker),
            (
                LossAction::ReconnectNow(AfterHello::ResendKept),
                vec![restart_error(4)]
            )
        );
        assert!(lock(&tracker).is_in_flight(&serde_json::json!(3)));
    }

    #[test]
    fn loss_before_init_without_kept_in_flight_waits() {
        let ping = b"{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"ping\"}\n";
        let tracker = tracker_with(&[INITIALIZE, ping], &[INIT_ERROR]);
        assert_eq!(
            loss(&tracker),
            (LossAction::WaitForRequest, vec![restart_error(6)])
        );
    }

    /// A connection whose peer reads one line, writes `script`, then collects the rest briefly.
    fn scripted_peer(
        script: &'static [&'static [u8]],
    ) -> (DaemonConn, tokio::task::JoinHandle<Vec<u8>>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (read, writer) = ours.into_split();
        let conn = DaemonConn {
            reader: BufReader::new(read),
            writer,
            key: "k".into(),
        };
        let peer = tokio::spawn(async move {
            let (read, mut write) = theirs.into_split();
            let mut reader = BufReader::new(read);
            let mut first = Vec::new();
            reader.read_until(b'\n', &mut first).await.unwrap();
            for line in script {
                write.write_all(line).await.unwrap();
            }
            let mut rest = Vec::new();
            let _ = tokio::time::timeout(
                Duration::from_millis(200),
                tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut rest),
            )
            .await;
            [first, rest].concat()
        });
        (conn, peer)
    }

    async fn replay_against(
        script: &'static [&'static [u8]],
        timeout: Duration,
    ) -> (Result<(), AttemptError>, Vec<u8>) {
        let tracker = tracker_with(&[INITIALIZE, INITIALIZED], &[]);
        let (mut conn, peer) = scripted_peer(script);
        let result = replay(&mut conn, &tracker, timeout).await;
        drop(conn);
        (result, peer.await.unwrap())
    }

    async fn forward_against(
        script: &'static [&'static [u8]],
        timeout: Duration,
    ) -> (Result<Vec<u8>, AttemptError>, Vec<u8>) {
        let tracker = tracker_with(&[INITIALIZE, INITIALIZED], &[]);
        let (mut conn, peer) = scripted_peer(script);
        let result = forward_initialize(&mut conn, &tracker, timeout).await;
        drop(conn);
        assert!(lock(&tracker).is_in_flight(&serde_json::json!(1)));
        (result, peer.await.unwrap())
    }

    #[tokio::test]
    async fn replay_drops_response_then_sends_initialized() {
        let (result, received) = replay_against(
            &[
                b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n",
                INIT_RESULT,
            ],
            Duration::from_secs(5),
        )
        .await;
        result.unwrap();
        assert_eq!(received, [INITIALIZE, INITIALIZED].concat());
    }

    #[tokio::test]
    async fn replay_error_response_is_retryable() {
        let (result, _) = replay_against(&[INIT_ERROR], Duration::from_secs(5)).await;
        assert!(result.unwrap_err().retryable);
    }

    #[tokio::test]
    async fn replay_timeout_is_not_retryable() {
        let (result, _) = replay_against(&[], Duration::from_millis(50)).await;
        assert!(!result.unwrap_err().retryable);
    }

    #[tokio::test]
    async fn forward_returns_response_and_sends_nothing_else() {
        for script in [&[INIT_RESULT], &[INIT_ERROR]] {
            let (result, received) = forward_against(script, Duration::from_secs(5)).await;
            assert_eq!(result.unwrap(), script[0]);
            assert_eq!(received, INITIALIZE);
        }
    }

    #[tokio::test]
    async fn forward_eof_is_retryable_and_timeout_is_not() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        let (read, writer) = ours.into_split();
        let mut conn = DaemonConn {
            reader: BufReader::new(read),
            writer,
            key: "k".into(),
        };
        let tracker = tracker_with(&[INITIALIZE], &[]);
        let eof = forward_initialize(&mut conn, &tracker, Duration::from_secs(5)).await;
        assert!(eof.unwrap_err().retryable);
        let (stalled, _) = forward_against(&[], Duration::from_millis(50)).await;
        assert!(!stalled.unwrap_err().retryable);
    }

    #[tokio::test]
    async fn replay_without_cached_initialize_is_fatal() {
        let tracker = SharedTracker::default();
        let (mut conn, _peer) = scripted_peer(&[]);
        let err = replay(&mut conn, &tracker, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(!err.retryable, "{err}");
    }

    #[tokio::test]
    async fn replay_marks_started_even_when_it_fails() {
        let tracker = tracker_with(&[INITIALIZE], &[INIT_RESULT]);
        lock(&tracker).set_link(Link::Reconnecting);
        assert_eq!(
            lock(&tracker).on_client_line(INITIALIZED),
            ClientLine::CacheOnly
        );
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        let (read, writer) = ours.into_split();
        let mut conn = DaemonConn {
            reader: BufReader::new(read),
            writer,
            key: "k".into(),
        };
        assert!(
            replay(&mut conn, &tracker, Duration::from_secs(1))
                .await
                .is_err()
        );
        assert_ne!(
            lock(&tracker).on_client_line(INITIALIZED),
            ClientLine::CacheOnly
        );
    }
}
