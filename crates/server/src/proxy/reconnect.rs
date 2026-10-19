use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::daemon::handshake::RefusalKind;
use crate::daemon::spawn::{AttemptError, Budgets, Connected, DaemonConn, RoundError, run_round};
use crate::proxy::track::{Link, Outcome, Tracker, request_id, response_outcome};
use crate::proxy::{
    SharedTracker, StdoutTx, connect, jsonrpc_error, lock, resolve_attempt, send_json,
};

pub const RESTART_CODE: i64 = -32603;
pub const RESTART_MESSAGE: &str = "ferrex daemon restarted, retry";

const MCP_HINT: &str = "run /mcp reconnect so Claude starts the new ferrex binary";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossAction {
    WaitForRequest,
    #[allow(dead_code, reason = "used by Task 13")]
    ReconnectNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayMode {
    /// Re-sends the cached `initialize` and discards its response.
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterHello {
    ReplayDrop,
    /// The caller flushes the queue straight after the hello.
    Nothing,
}

/// What a reconnect round needs beyond the stdin side of the proxy.
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
    /// The error message for requests a failed round leaves unanswered.
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

/// Answers every in-flight request with the restart error; the daemon will never answer them.
pub fn on_daemon_loss(tracker: &SharedTracker, out: &StdoutTx) -> LossAction {
    answer_in_flight(&mut lock(tracker), out, RESTART_MESSAGE);
    LossAction::WaitForRequest
}

/// Answers every in-flight request, queued ones included, with `message`; the link goes down.
pub fn fail_pending(tracker: &SharedTracker, out: &StdoutTx, message: &str) {
    let mut tracker = lock(tracker);
    answer_in_flight(&mut tracker, out, message);
    tracker.set_link(Link::Down);
}

fn answer_in_flight(tracker: &mut Tracker, out: &StdoutTx, message: &str) {
    for id in tracker.take_in_flight() {
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
                    replay(&mut conn, tracker, ReplayMode::Drop, budgets.replay).await?
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
    mode: ReplayMode,
    timeout: Duration,
) -> Result<Option<Vec<u8>>, AttemptError> {
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
    write_replayed(&mut conn.writer, &initialize).await?;
    tokio::time::timeout(timeout, read_response(&mut conn.reader, &id))
        .await
        .map_err(|_| {
            AttemptError::fatal(format!(
                "daemon did not answer the replayed initialize within {timeout:?}"
            ))
        })??;
    match mode {
        ReplayMode::Drop => {}
    }
    if let Some(initialized) = initialized {
        write_replayed(&mut conn.writer, &initialized).await?;
    }
    Ok(None)
}

async fn write_replayed(writer: &mut OwnedWriteHalf, line: &[u8]) -> Result<(), AttemptError> {
    writer
        .write_all(line)
        .await
        .map_err(|e| AttemptError::retryable(format!("replay write failed: {e}")))
}

/// Reads up to the response to `id`, skipping anything else the daemon sends first.
async fn read_response(
    reader: &mut BufReader<OwnedReadHalf>,
    id: &serde_json::Value,
) -> Result<Vec<u8>, AttemptError> {
    loop {
        let mut line = Vec::new();
        let n = reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|e| AttemptError::retryable(format!("replay read failed: {e}")))?;
        if n == 0 || !line.ends_with(b"\n") {
            return Err(AttemptError::retryable(
                "daemon closed the connection during the initialize replay",
            ));
        }
        match response_outcome(&line, id) {
            Some(Outcome::Success) => return Ok(line),
            Some(Outcome::Error) => {
                return Err(AttemptError::retryable(
                    "daemon answered the replayed initialize with an error",
                ));
            }
            None => {}
        }
    }
}

#[cfg(test)]
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
        while let Ok(Out::Line(line)) = rx.try_recv() {
            lines.push(serde_json::from_slice(&line).unwrap());
        }
        lines
    }

    #[test]
    fn on_daemon_loss_answers_and_clears_in_flight() {
        let tracker = SharedTracker::default();
        lock(&tracker)
            .on_client_line(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\"}\n");
        let (out, mut rx) = mpsc::unbounded_channel();
        assert_eq!(on_daemon_loss(&tracker, &out), LossAction::WaitForRequest);
        assert_eq!(
            drain(&mut rx),
            vec![serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "error": {"code": RESTART_CODE, "message": RESTART_MESSAGE},
            })]
        );
        assert!(lock(&tracker).take_in_flight().is_empty());
    }

    async fn replay_against(
        script: &'static [&'static [u8]],
        timeout: Duration,
    ) -> (Result<Option<Vec<u8>>, AttemptError>, Vec<u8>) {
        let tracker = SharedTracker::default();
        lock(&tracker)
            .on_client_line(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n");
        lock(&tracker)
            .on_client_line(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n");
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (read, writer) = ours.into_split();
        let mut conn = DaemonConn {
            reader: BufReader::new(read),
            writer,
            key: "k".into(),
        };
        let daemon = tokio::spawn(async move {
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
        let result = replay(&mut conn, &tracker, ReplayMode::Drop, timeout).await;
        drop(conn);
        (result, daemon.await.unwrap())
    }

    #[tokio::test]
    async fn replay_drops_response_then_sends_initialized() {
        let (result, received) = replay_against(
            &[
                b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n",
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n",
            ],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(result.unwrap(), None);
        assert_eq!(
            received,
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n\
              {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n"
        );
    }

    #[tokio::test]
    async fn replay_error_response_is_retryable() {
        let (result, _) = replay_against(
            &[b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":1,\"message\":\"x\"}}\n"],
            Duration::from_secs(5),
        )
        .await;
        assert!(result.unwrap_err().retryable);
    }

    #[tokio::test]
    async fn replay_timeout_is_not_retryable() {
        let (result, _) = replay_against(&[], Duration::from_millis(50)).await;
        assert!(!result.unwrap_err().retryable);
    }

    #[tokio::test]
    async fn replay_without_cached_initialize_fails_but_marks_started() {
        let tracker = SharedTracker::default();
        lock(&tracker).set_link(Link::Reconnecting);
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let (read, writer) = ours.into_split();
        let mut conn = DaemonConn {
            reader: BufReader::new(read),
            writer,
            key: "k".into(),
        };
        let err = replay(
            &mut conn,
            &tracker,
            ReplayMode::Drop,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(!err.retryable, "{err}");
        let initialized = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        assert_ne!(
            lock(&tracker).on_client_line(initialized),
            ClientLine::CacheOnly
        );
    }
}
