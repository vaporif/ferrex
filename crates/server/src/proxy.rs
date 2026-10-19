mod track;

use std::collections::VecDeque;
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
use tokio::sync::{mpsc, oneshot};

use crate::cli::{Cli, build_config};
use crate::daemon::addr::{DaemonPaths, current_exe_canonical};
use crate::daemon::handshake::Hello;
use crate::daemon::spawn::{
    AttemptError, Budgets, DaemonConn, Resolved, connect_or_spawn, run_round, send_hello,
};
use crate::proxy::track::Tracker;

pub type SharedTracker = Arc<Mutex<Tracker>>;

const STDIN_CHANNEL: usize = 64;
const EXIT_FALLBACK: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum StdinEvent {
    /// One client line, always ending in `\n`.
    Line(Vec<u8>),
    Eof,
}

#[derive(Debug)]
enum Out {
    Line(Vec<u8>),
    Exit(i32),
}

type StdoutTx = mpsc::UnboundedSender<Out>;

/// Proxies the client's stdio to the shared daemon, spawning it if needed; never returns once
/// stdio is set up.
pub fn run(config: FerrexConfig, argv: &[OsString]) -> eyre::Result<()> {
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
    let tracker = SharedTracker::default();
    let mut queue = VecDeque::new();
    let budgets = Budgets::from_env();
    let budgets = &budgets;
    let mut initial = Some(config);
    let resolve = || resolve_attempt(initial.take(), argv, exe);
    let attempt = move |resolved| connect(resolved, exe, argv, budgets);
    let round = run_round(resolve, attempt, 0);
    let conn = match wait_with_stdin(round, &mut stdin, &tracker, &mut queue).await {
        None => exit_via_writer(out, 0),
        Some(Err(e)) => {
            diag(&format!("ferrex: could not reach the daemon: {}", e.last));
            exit_via_writer(out, 1)
        }
        Some(Ok(connected)) => connected.value,
    };
    match forward(conn, &mut stdin, &tracker, queue, out).await {
        Ended::StdinEof => exit_via_writer(out, 0),
        Ended::DaemonLost => {
            diag("ferrex daemon connection lost");
            exit_via_writer(out, 1)
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    StdinEof,
    DaemonLost,
}

async fn forward(
    conn: DaemonConn,
    stdin: &mut mpsc::Receiver<StdinEvent>,
    tracker: &SharedTracker,
    queue: VecDeque<Vec<u8>>,
    out: &StdoutTx,
) -> Ended {
    let DaemonConn {
        reader, mut writer, ..
    } = conn;
    let (lost_tx, mut lost) = oneshot::channel();
    let pump = tokio::spawn(pump_daemon(
        reader,
        out.clone(),
        Arc::clone(tracker),
        lost_tx,
    ));
    let ended = 'forward: {
        for line in queue {
            if write_line(&mut writer, &line).await.is_err() {
                break 'forward Ended::DaemonLost;
            }
        }
        loop {
            tokio::select! {
                event = stdin.recv() => match event {
                    Some(StdinEvent::Line(line)) => {
                        lock(tracker).on_client_line(&line);
                        if write_line(&mut writer, &line).await.is_err() {
                            break 'forward Ended::DaemonLost;
                        }
                    }
                    Some(StdinEvent::Eof) | None => break 'forward Ended::StdinEof,
                },
                _ = &mut lost => break 'forward Ended::DaemonLost,
            }
        }
    };
    pump.abort();
    ended
}

async fn write_line(writer: &mut OwnedWriteHalf, line: &[u8]) -> io::Result<()> {
    writer.write_all(line).await.inspect_err(|e| {
        tracing::debug!(error = %e, "write to daemon failed");
    })
}

/// Copies whole daemon lines to stdout until the socket closes, then reports the loss.
async fn pump_daemon(
    mut reader: tokio::io::BufReader<OwnedReadHalf>,
    out: StdoutTx,
    tracker: SharedTracker,
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
        lock(&tracker).on_daemon_line(&line);
        if out.send(Out::Line(line)).is_err() {
            break;
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
                    lock(tracker).on_client_line(&line);
                    queue.push_back(line);
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
                    Out::Line(line) => {
                        if let Err(e) = stdout.write_all(&line).and_then(|()| stdout.flush()) {
                            exit_on_stdout_error(&e);
                        }
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
    flush_and_exit(code)
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

#[allow(dead_code, reason = "used by Task 11")]
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
mod tests {
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
