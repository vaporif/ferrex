mod budget;
mod reconnect;
mod track;

use std::collections::VecDeque;
use std::convert::Infallible;
use std::ffi::OsString;
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use clap::Parser;
use ferrex_core::FerrexConfig;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Notify, OwnedSemaphorePermit, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::cli::{Cli, build_config};
use crate::daemon::addr::{DaemonPaths, current_exe_canonical};
use crate::daemon::handshake::Hello;
use crate::daemon::spawn::{
    AttemptError, Budgets, Connected, DaemonConn, MAX_ATTEMPTS, Resolved, RoundError,
    connect_or_spawn, run_round, send_hello,
};
use crate::proxy::budget::StdoutBudget;
use crate::proxy::reconnect::{
    AfterHello, LossAction, ReconnectCtx, ResendPending, exe_mtime, fail_pending, on_daemon_loss,
    reconnect,
};
use crate::proxy::track::{ClientLine, Link, Tracker};

pub type SharedTracker = Arc<Mutex<Tracker>>;

const STDIN_CHANNEL: usize = 64;
const EXIT_FALLBACK: Duration = Duration::from_secs(1);
const LIST_CHANGED: &str = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}";

#[derive(Debug)]
pub enum StdinEvent {
    /// One client line, always ending in `\n`.
    Line(Vec<u8>),
    Eof,
}

#[derive(Debug)]
enum Out {
    /// A daemon line carries its share of the stdout budget, returned once it is written.
    Line(Vec<u8>, Option<OwnedSemaphorePermit>),
    Exit(i32),
}

/// Unbounded so synthetic lines and `Exit` never block; daemon lines are metered by a budget.
type StdoutTx = mpsc::UnboundedSender<Out>;

#[derive(Debug)]
pub enum ConnState {
    Connected {
        writer: OwnedWriteHalf,
        lost: oneshot::Receiver<()>,
        reader: JoinHandle<()>,
        key: String,
    },
    Disconnected,
    /// A round is due; `used` counts the attempts a re-send pending at the loss already took.
    Reconnecting {
        after: AfterHello,
        used: u8,
    },
}

/// Proxies the client's stdio to the shared daemon, spawning it if needed; never returns once
/// stdio is set up.
pub fn run(config: FerrexConfig, argv: &[OsString]) -> eyre::Result<Infallible> {
    let exe = current_exe_canonical()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let stdin = spawn_stdin_thread()?;
    let out = spawn_stdout_writer()?;
    runtime.block_on(proxy(config, argv, &exe, stdin, &out))
}

async fn proxy(
    config: FerrexConfig,
    argv: &[OsString],
    exe: &Path,
    mut stdin: mpsc::Receiver<StdinEvent>,
    out: &StdoutTx,
) -> ! {
    let startup_mtime = exe_mtime(exe);
    let tracker = SharedTracker::default();
    let mut queue = VecDeque::new();
    let budgets = Budgets::from_env();
    let mut initial = Some(config);
    let resolve = || resolve_attempt(initial.take(), argv, exe);
    let attempt = |resolved| connect(resolved, exe, argv, &budgets);
    let round = run_round(resolve, attempt, 0);
    let conn = match wait_with_stdin(round, &mut stdin, &tracker, &mut queue).await {
        None => exit_via_writer(out, 0),
        Some(Err(e)) => {
            diag(&format!("ferrex: could not reach the daemon: {}", e.last));
            exit_via_writer(out, 1)
        }
        Some(Ok(connected)) => connected.value,
    };
    let ctx = ReconnectCtx {
        exe: exe.to_owned(),
        argv: argv.to_vec(),
        budgets,
        startup_mtime,
        last_key: conn.key.clone(),
        tracker,
    };
    Proxy {
        ctx,
        stdin,
        queue,
        out,
        budget: Arc::default(),
        initialised_at_loss: false,
        pending: None,
        answered: Arc::default(),
    }
    .run(conn)
    .await
}

/// The first attempt reuses the config `main` built; later ones rebuild it from `argv`.
fn resolve_attempt(
    config: Option<FerrexConfig>,
    argv: &[OsString],
    exe: &Path,
) -> Result<Resolved, AttemptError> {
    let config = config.map_or_else(|| rebuild_config(argv), Ok)?;
    let paths = DaemonPaths::for_config(&config, exe)
        .map_err(|e| AttemptError::retryable(format!("{e:#}")))?;
    Ok(Resolved { config, paths })
}

fn rebuild_config(argv: &[OsString]) -> Result<FerrexConfig, AttemptError> {
    let program = std::iter::once(OsString::from("ferrex"));
    let cli = Cli::try_parse_from(program.chain(argv.iter().cloned()))
        .map_err(|e| AttemptError::retryable(format!("parse arguments: {e}")))?;
    build_config(cli).map_err(|e| AttemptError::retryable(format!("{e:#}")))
}

async fn connect(
    resolved: Resolved,
    exe: &Path,
    argv: &[OsString],
    budgets: &Budgets,
) -> Result<DaemonConn, AttemptError> {
    let Resolved { config, paths } = resolved;
    let stream = connect_or_spawn(&paths, exe, argv, budgets).await?;
    send_hello(
        stream,
        &Hello::new(paths.key, config.namespace),
        budgets.hello,
    )
    .await
}

struct Proxy<'a> {
    ctx: ReconnectCtx,
    stdin: mpsc::Receiver<StdinEvent>,
    queue: VecDeque<Vec<u8>>,
    out: &'a StdoutTx,
    budget: Arc<StdoutBudget>,
    initialised_at_loss: bool,
    pending: Option<ResendPending>,
    /// Woken whenever a daemon response leaves no request in flight.
    answered: Arc<Notify>,
}

impl Proxy<'_> {
    async fn run(mut self, conn: DaemonConn) -> ! {
        let mut state = self.connected(conn);
        loop {
            state = match state {
                ConnState::Connected {
                    mut writer,
                    mut lost,
                    reader,
                    key,
                } => {
                    if self.flush_queue(&mut writer).await.is_ok() {
                        self.forward(&mut writer, &mut lost).await;
                    }
                    self.on_loss(reader, &key).await
                }
                ConnState::Disconnected => self.await_request().await,
                ConnState::Reconnecting { after, used } => self.reconnect_round(after, used).await,
            };
        }
    }

    fn connected(&self, conn: DaemonConn) -> ConnState {
        let DaemonConn {
            reader,
            writer,
            key,
        } = conn;
        lock(&self.ctx.tracker).set_link(Link::Up);
        let (lost_tx, lost) = oneshot::channel();
        let reader = tokio::spawn(pump_daemon(
            reader,
            self.out.clone(),
            Arc::clone(&self.budget),
            Arc::clone(&self.ctx.tracker),
            Arc::clone(&self.answered),
            lost_tx,
        ));
        ConnState::Connected {
            writer,
            lost,
            reader,
            key,
        }
    }

    async fn flush_queue(&mut self, writer: &mut OwnedWriteHalf) -> io::Result<()> {
        while let Some(line) = self.queue.pop_front() {
            write_line(writer, &line).await?;
        }
        Ok(())
    }

    async fn forward(&mut self, writer: &mut OwnedWriteHalf, lost: &mut oneshot::Receiver<()>) {
        loop {
            tokio::select! {
                event = self.stdin.recv() => match event {
                    Some(StdinEvent::Line(line)) => {
                        lock(&self.ctx.tracker).on_client_line(&line);
                        if write_line(writer, &line).await.is_err() {
                            return;
                        }
                    }
                    Some(StdinEvent::Eof) | None => {
                        self.drain(lost).await;
                        exit_via_writer(self.out, 0)
                    }
                },
                _ = &mut *lost => return,
            }
        }
    }

    /// Gives the daemon up to the drain budget to answer requests sent before stdin closed.
    async fn drain(&self, lost: &mut oneshot::Receiver<()>) {
        let answered = async {
            loop {
                // Created before the check, so a notify in between isn't missed.
                let notified = self.answered.notified();
                if !lock(&self.ctx.tracker).has_in_flight() {
                    return;
                }
                notified.await;
            }
        };
        tokio::select! {
            () = answered => {}
            _ = lost => {}
            // A client that has stopped reading stdout can never receive what we'd wait for.
            () = self.budget.stalled() => {}
            () = tokio::time::sleep(self.ctx.budgets.drain) => {
                diag("ferrex: stdin closed with requests still unanswered; exiting");
            }
        }
    }

    async fn on_loss(&mut self, reader: JoinHandle<()>, key: &str) -> ConnState {
        reader.abort();
        // Once it has stopped it can't forward a response whose id gets the restart error.
        let _ = reader.await;
        // Nothing was queued after it, so forwarding it now keeps stdout in daemon order.
        if let Some(line) = self.budget.take_held() {
            lock(&self.ctx.tracker).on_daemon_line(&line);
            send_line(self.out, line);
        }
        self.queue.clear();
        let used = {
            let mut tracker = lock(&self.ctx.tracker);
            tracker.set_link(Link::Down);
            self.initialised_at_loss = tracker.initialised();
            self.pending
                .take()
                .map_or(0, |p| p.attempts_carried(&tracker))
        };
        match on_daemon_loss(&self.ctx.tracker, self.out) {
            LossAction::WaitForRequest => {
                diag(&format!(
                    "ferrex: daemon connection lost (key {key}); reconnecting on the next request"
                ));
                ConnState::Disconnected
            }
            LossAction::ReconnectNow(_) if used >= MAX_ATTEMPTS => {
                diag(&format!(
                    "ferrex daemon connection lost again while the re-sent first request was pending (after {used} attempts)"
                ));
                exit_via_writer(self.out, 1)
            }
            LossAction::ReconnectNow(after) => {
                diag(&format!(
                    "ferrex: daemon connection lost (key {key}) during startup; reconnecting now"
                ));
                lock(&self.ctx.tracker).set_link(Link::Reconnecting);
                ConnState::Reconnecting { after, used }
            }
        }
    }

    /// Drops lines until the client's next request, which starts the reconnect.
    async fn await_request(&mut self) -> ConnState {
        loop {
            let Some(StdinEvent::Line(line)) = self.stdin.recv().await else {
                exit_via_writer(self.out, 0)
            };
            let mut tracker = lock(&self.ctx.tracker);
            if tracker.on_client_line(&line) == ClientLine::Request {
                tracker.set_link(Link::Reconnecting);
                let after = if tracker.replays_handshake() {
                    AfterHello::ReplayDrop
                } else {
                    AfterHello::Nothing
                };
                drop(tracker);
                self.queue.push_back(line);
                return ConnState::Reconnecting { after, used: 0 };
            }
        }
    }

    async fn reconnect_round(&mut self, after: AfterHello, used: u8) -> ConnState {
        let round = reconnect(&self.ctx, after, used);
        let tracker = &self.ctx.tracker;
        match wait_with_stdin(round, &mut self.stdin, tracker, &mut self.queue).await {
            None => exit_via_writer(self.out, 0),
            Some(Err(e)) => self.round_failed(&e),
            Some(Ok(Connected {
                value: (conn, response),
                attempts_used,
            })) => {
                self.announce_key(&conn.key);
                if let Some(line) = response {
                    lock(&self.ctx.tracker).on_daemon_line(&line);
                    send_line(self.out, line);
                }
                if after == AfterHello::ResendKept {
                    self.pending = lock(&self.ctx.tracker)
                        .kept_first()
                        .map(|kept| ResendPending::new(kept, attempts_used));
                }
                self.connected(conn)
            }
        }
    }

    fn announce_key(&mut self, key: &str) {
        if self.initialised_at_loss && key != self.ctx.last_key {
            send_json(self.out, LIST_CHANGED.to_owned());
        }
        key.clone_into(&mut self.ctx.last_key);
    }

    fn round_failed(&mut self, e: &RoundError) -> ConnState {
        if !lock(&self.ctx.tracker).initialised() {
            diag(&format!("ferrex: could not reach the daemon: {}", e.last));
            exit_via_writer(self.out, 1)
        }
        fail_pending(&self.ctx.tracker, self.out, &self.ctx.failure_message(e));
        self.queue.clear();
        ConnState::Disconnected
    }
}

async fn write_line(writer: &mut OwnedWriteHalf, line: &[u8]) -> io::Result<()> {
    writer.write_all(line).await.inspect_err(|e| {
        tracing::debug!(error = %e, "write to daemon failed");
    })
}

fn send_json(out: &StdoutTx, line: String) {
    let mut line = line.into_bytes();
    line.push(b'\n');
    send_line(out, line);
}

/// The writer thread only stops by exiting the process, so a send error needs no handling.
fn send_line(out: &StdoutTx, line: Vec<u8>) {
    let _ = out.send(Out::Line(line, None));
}

/// Waiting on `budget` stops reads, so an unread stdout back-pressures the daemon socket.
async fn pump_daemon(
    mut reader: tokio::io::BufReader<OwnedReadHalf>,
    out: StdoutTx,
    budget: Arc<StdoutBudget>,
    tracker: SharedTracker,
    answered: Arc<Notify>,
    lost: oneshot::Sender<()>,
) {
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => break,
            // A line cut short by EOF is never forwarded, so stdout only ever sees whole lines.
            Ok(_) if !line.ends_with(b"\n") => break,
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(error = %e, "read from daemon failed");
                break;
            }
        }
        let Some(credit) = budget.hold_until_free(line).await else {
            break;
        };
        let Some(line) = budget.take_held() else {
            break;
        };
        lock(&tracker).on_daemon_line(&line);
        if out.send(Out::Line(line, Some(credit))).is_err() {
            break;
        }
        // After the send, so an exit the notify triggers is queued behind this line.
        if !lock(&tracker).has_in_flight() {
            answered.notify_waiters();
        }
    }
    let _ = lost.send(());
}

/// Runs `fut` while queueing and tracking client lines; `None` if stdin hits EOF first.
pub async fn wait_with_stdin<F: Future>(
    fut: F,
    rx: &mut mpsc::Receiver<StdinEvent>,
    tracker: &SharedTracker,
    queue: &mut VecDeque<Vec<u8>>,
) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    loop {
        tokio::select! {
            biased;
            output = &mut fut => return Some(output),
            event = rx.recv() => match event {
                Some(StdinEvent::Line(line)) => {
                    if lock(tracker).on_client_line(&line) != ClientLine::CacheOnly {
                        queue.push_back(line);
                    }
                }
                Some(StdinEvent::Eof) | None => return None,
            },
        }
    }
}

fn lock(tracker: &SharedTracker) -> MutexGuard<'_, Tracker> {
    tracker.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads stdin on a plain thread: tokio's stdin read can't be cancelled and would block exit.
fn spawn_stdin_thread() -> io::Result<mpsc::Receiver<StdinEvent>> {
    let (tx, rx) = mpsc::channel(STDIN_CHANNEL);
    std::thread::Builder::new()
        .name("ferrex-stdin".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            loop {
                let mut line = Vec::new();
                match stdin.read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if !line.ends_with(b"\n") {
                            line.push(b'\n');
                        }
                        if tx.blocking_send(StdinEvent::Line(line)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "stdin read failed, treating it as EOF");
                        break;
                    }
                }
            }
            let _ = tx.blocking_send(StdinEvent::Eof);
        })?;
    Ok(rx)
}

/// The single owner of stdout, on its own thread so a blocked write never stalls the runtime.
fn spawn_stdout_writer() -> io::Result<StdoutTx> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("ferrex-stdout".into())
        .spawn(move || {
            let mut stdout = io::stdout().lock();
            while let Some(msg) = rx.blocking_recv() {
                match msg {
                    Out::Line(line, credit) => {
                        if let Err(e) = stdout.write_all(&line).and_then(|()| stdout.flush()) {
                            exit_on_stdout_error(&e);
                        }
                        drop(credit);
                    }
                    Out::Exit(code) => {
                        let _ = stdout.flush();
                        flush_and_exit(code);
                    }
                }
            }
        })?;
    Ok(tx)
}

fn exit_on_stdout_error(e: &io::Error) -> ! {
    if e.kind() == io::ErrorKind::BrokenPipe {
        flush_and_exit(0);
    }
    diag(&format!("ferrex: write to stdout failed: {e}"));
    flush_and_exit(1)
}

/// Exits after the writer has written every line queued before this call.
fn exit_via_writer(out: &StdoutTx, code: i32) -> ! {
    if out.send(Out::Exit(code)).is_ok() {
        std::thread::sleep(EXIT_FALLBACK);
    }
    // Not flushing stdout: a writer blocked on a full pipe holds its lock, so that would hang.
    let _ = io::stderr().flush();
    std::process::exit(code)
}

fn flush_and_exit(code: i32) -> ! {
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    std::process::exit(code)
}

/// Writes a diagnostic line to stderr, which the client may have closed: errors are ignored.
fn diag(message: &str) {
    let _ = writeln!(io::stderr(), "{message}");
}

#[must_use]
pub fn jsonrpc_error(id: &Value, code: i64, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
    .to_string()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[test]
    fn jsonrpc_error_shape() {
        let line = jsonrpc_error(&json!("a"), -32603, "ferrex daemon restarted, retry");
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({
                "jsonrpc": "2.0",
                "id": "a",
                "error": {"code": -32603, "message": "ferrex daemon restarted, retry"},
            })
        );
    }

    #[tokio::test]
    async fn wait_with_stdin_queues_and_tracks_lines() {
        let (tx, mut rx) = mpsc::channel(4);
        let tracker = SharedTracker::default();
        let mut queue = VecDeque::new();
        let init = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n".to_vec();
        tx.send(StdinEvent::Line(init.clone())).await.unwrap();
        let (done_tx, done_rx) = oneshot::channel::<u8>();
        let fut = async move { done_rx.await.unwrap() };
        let waiter = async { wait_with_stdin(fut, &mut rx, &tracker, &mut queue).await };
        let finisher = async {
            tokio::task::yield_now().await;
            done_tx.send(7).unwrap();
        };
        let (output, ()) = tokio::join!(waiter, finisher);
        assert_eq!(output, Some(7));
        assert_eq!(queue, VecDeque::from([init.clone()]));
        assert_eq!(lock(&tracker).cached_initialize(), Some(&init[..]));
    }

    #[tokio::test]
    async fn wait_with_stdin_returns_none_on_eof() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(StdinEvent::Eof).await.unwrap();
        let tracker = SharedTracker::default();
        let mut queue = VecDeque::new();
        let output =
            wait_with_stdin(std::future::pending::<()>(), &mut rx, &tracker, &mut queue).await;
        assert_eq!(output, None);
    }
}
