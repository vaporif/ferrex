use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use ferrex_core::FerrexConfig;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, Command};
use tokio::time::Instant;

use crate::daemon::addr::{DaemonPaths, ensure_run_dir};
use crate::daemon::handshake::{Hello, HelloReply, RefusalKind};

pub const MAX_ATTEMPTS: u8 = 3;
pub const BACKOFF: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(1)];

const LOCK_POLL: Duration = Duration::from_millis(50);
const ACCEPT_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Budgets {
    pub flock: Duration,
    pub accept: Duration,
    pub hello: Duration,
    pub replay: Duration,
}

impl Budgets {
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var("FERREX_TEST_PROXY_TIMEOUT_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map_or(
                Self {
                    flock: Duration::from_secs(300),
                    accept: Duration::from_secs(30),
                    hello: Duration::from_secs(3600),
                    replay: Duration::from_secs(30),
                },
                |ms| {
                    let all = Duration::from_millis(ms);
                    Self {
                        flock: all,
                        accept: all,
                        hello: all,
                        replay: all,
                    }
                },
            )
    }
}

#[derive(Debug)]
pub struct AttemptError {
    pub kind: Option<RefusalKind>,
    pub retryable: bool,
    pub child_exited_early: bool,
    pub message: String,
}

impl AttemptError {
    #[must_use]
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            kind: None,
            retryable: true,
            child_exited_early: false,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            kind: None,
            retryable: false,
            child_exited_early: false,
            message: message.into(),
        }
    }
}

impl fmt::Display for AttemptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AttemptError {}

#[derive(Debug)]
pub struct Resolved {
    pub config: FerrexConfig,
    pub paths: DaemonPaths,
}

#[derive(Debug)]
pub struct Connected<T> {
    pub value: T,
    pub attempts_used: u8,
}

#[derive(Debug)]
pub struct RoundError {
    pub last: AttemptError,
    pub any_child_exited_early: bool,
}

#[derive(Debug)]
pub struct DaemonConn {
    pub reader: BufReader<OwnedReadHalf>,
    pub writer: OwnedWriteHalf,
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectClass {
    Stale,
    Busy,
    Other,
}

fn classify_connect_error(e: &io::Error) -> ConnectClass {
    match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => ConnectClass::Stale,
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::TimedOut => {
            ConnectClass::Busy
        }
        // macOS answers ENOTSOCK for a regular file at the path, where Linux says ECONNREFUSED.
        _ if e.raw_os_error() == Some(rustix::io::Errno::NOTSOCK.raw_os_error()) => {
            ConnectClass::Stale
        }
        _ => ConnectClass::Other,
    }
}

fn log_hint(log_file: &Path, env_log: Option<&OsStr>) -> String {
    env_log.map_or_else(
        || format!("see {}", log_file.display()),
        |env_log| {
            format!(
                "see {} and {}",
                log_file.display(),
                Path::new(env_log).display()
            )
        },
    )
}

fn daemon_log_hint(paths: &DaemonPaths) -> String {
    log_hint(
        &paths.log_file,
        std::env::var_os("FERREX_LOG_FILE").as_deref(),
    )
}

pub async fn connect_or_spawn(
    paths: &DaemonPaths,
    exe: &Path,
    argv: &[OsString],
    budgets: &Budgets,
) -> Result<UnixStream, AttemptError> {
    if let Ok(stream) = UnixStream::connect(&paths.socket).await {
        return Ok(stream);
    }
    ensure_run_dir(&paths.run_dir).map_err(|e| AttemptError::fatal(e.to_string()))?;
    let lock = acquire_lock(&paths.lock_file, budgets.flock).await?;
    match UnixStream::connect(&paths.socket).await {
        Ok(stream) => return Ok(stream),
        Err(e) => match classify_connect_error(&e) {
            ConnectClass::Stale => remove_stale_socket(&paths.socket)?,
            ConnectClass::Busy => {
                return Err(AttemptError::retryable(format!(
                    "daemon at {} is busy: {e}",
                    paths.socket.display()
                )));
            }
            ConnectClass::Other => {
                return Err(AttemptError::retryable(format!(
                    "connect {}: {e}",
                    paths.socket.display()
                )));
            }
        },
    }
    let guard = SpawnGuard {
        child: Some(spawn_daemon(paths, exe, argv)?),
        lock: Some(lock),
    };
    wait_for_accept(guard, paths, budgets.accept).await
}

async fn acquire_lock(path: &Path, budget: Duration) -> Result<File, AttemptError> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| AttemptError::retryable(format!("open {}: {e}", path.display())))?;
    let deadline = Instant::now() + budget;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => {
                return Err(AttemptError::retryable(format!(
                    "lock {}: {e}",
                    path.display()
                )));
            }
        }
        if Instant::now() >= deadline {
            return Err(AttemptError::fatal(format!(
                "timed out waiting for spawn lock {}",
                path.display()
            )));
        }
        tokio::time::sleep(LOCK_POLL).await;
    }
}

fn remove_stale_socket(socket: &Path) -> Result<(), AttemptError> {
    match std::fs::remove_file(socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(AttemptError::retryable(format!(
            "remove stale socket {}: {e}",
            socket.display()
        ))),
        _ => Ok(()),
    }
}

fn spawn_daemon(paths: &DaemonPaths, exe: &Path, argv: &[OsString]) -> Result<Child, AttemptError> {
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&paths.log_file)
        .map_err(|e| AttemptError::retryable(format!("open {}: {e}", paths.log_file.display())))?;
    let mut cmd = Command::new(exe);
    cmd.args(argv)
        .args(["daemon", "--key", &paths.key])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: the closure runs between fork and exec and only calls setsid, which is
    // async-signal-safe and does not allocate.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid()?;
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| AttemptError::retryable(format!("spawn {}: {e}", exe.display())))
}

async fn wait_for_accept(
    mut guard: SpawnGuard,
    paths: &DaemonPaths,
    budget: Duration,
) -> Result<UnixStream, AttemptError> {
    let deadline = Instant::now() + budget;
    loop {
        if let Ok(stream) = UnixStream::connect(&paths.socket).await {
            guard.disarm();
            return Ok(stream);
        }
        match guard.try_wait() {
            Ok(Some(status)) => {
                return Err(AttemptError {
                    kind: None,
                    retryable: true,
                    child_exited_early: true,
                    message: format!(
                        "daemon exited before accepting connections ({status}); {}",
                        daemon_log_hint(paths)
                    ),
                });
            }
            Ok(None) => {}
            Err(e) => tracing::debug!(error = %e, "try_wait on spawned daemon failed"),
        }
        if Instant::now() >= deadline {
            return Err(AttemptError::retryable(format!(
                "daemon did not accept connections on {} within {budget:?}; {}",
                paths.socket.display(),
                daemon_log_hint(paths)
            )));
        }
        tokio::time::sleep(ACCEPT_POLL).await;
    }
}

// Dropping the guard kills the child before releasing the lock, so a late bind cannot race a new spawn.
#[derive(Debug)]
struct SpawnGuard {
    child: Option<Child>,
    lock: Option<File>,
}

impl SpawnGuard {
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.as_mut().map_or(Ok(None), Child::try_wait)
    }

    fn disarm(mut self) {
        if let Some(child) = self.child.take() {
            reap(child);
        }
        self.release_lock();
    }

    fn release_lock(&mut self) {
        if let Some(lock) = self.lock.take()
            && let Err(e) = lock.unlock()
        {
            tracing::debug!(error = %e, "unlock spawn lock failed");
        }
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Err(e) = child.start_kill() {
                tracing::debug!(error = %e, "kill spawned daemon failed");
            }
            reap(child);
        }
        self.release_lock();
    }
}

fn reap(mut child: Child) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = child.wait().await;
        });
    }
}

pub async fn send_hello(
    stream: UnixStream,
    hello: &Hello,
    timeout: Duration,
) -> Result<DaemonConn, AttemptError> {
    let (read, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let exchange = async {
        writer.write_all(hello.encode().as_bytes()).await?;
        reader.read_line(&mut line).await
    };
    match tokio::time::timeout(timeout, exchange).await {
        Err(_) => {
            return Err(AttemptError::fatal(format!(
                "daemon did not answer the hello within {timeout:?}"
            )));
        }
        Ok(Err(e)) => {
            return Err(AttemptError::retryable(format!(
                "hello exchange failed: {e}"
            )));
        }
        Ok(Ok(0)) => {
            return Err(AttemptError::retryable(
                "daemon closed the connection before replying to the hello",
            ));
        }
        Ok(Ok(_)) if !line.ends_with('\n') => {
            return Err(AttemptError::retryable(
                "daemon closed the connection mid-reply to the hello",
            ));
        }
        Ok(Ok(_)) => {}
    }
    let reply = HelloReply::decode(&line).map_err(|message| AttemptError {
        kind: Some(RefusalKind::Protocol),
        retryable: false,
        child_exited_early: false,
        message,
    })?;
    if reply.ok {
        return Ok(DaemonConn {
            reader,
            writer,
            key: hello.key.clone(),
        });
    }
    let kind = reply.kind.unwrap_or(RefusalKind::Protocol);
    Err(AttemptError {
        kind: Some(kind),
        retryable: matches!(kind, RefusalKind::Key | RefusalKind::Session),
        child_exited_early: false,
        message: format!(
            "daemon refused the hello ({kind:?}): {}",
            reply.error.as_deref().unwrap_or("no reason given")
        ),
    })
}

pub async fn run_round<T, R, A, Fut>(
    mut resolve: R,
    mut attempt: A,
    used: u8,
) -> Result<Connected<T>, RoundError>
where
    R: FnMut() -> Result<Resolved, AttemptError>,
    A: FnMut(Resolved) -> Fut,
    Fut: Future<Output = Result<T, AttemptError>>,
{
    debug_assert!(
        used < MAX_ATTEMPTS,
        "round started with {used} attempts used"
    );
    let mut attempts = used;
    let mut any_child_exited_early = false;
    loop {
        if let Some(prev) = attempts.checked_sub(1) {
            let backoff = BACKOFF[usize::from(prev).min(BACKOFF.len() - 1)];
            tokio::time::sleep(backoff).await;
        }
        attempts += 1;
        let outcome = match resolve() {
            Ok(resolved) => attempt(resolved).await,
            Err(e) => Err(AttemptError::retryable(e.message)),
        };
        let last = match outcome {
            Ok(value) => {
                return Ok(Connected {
                    value,
                    attempts_used: attempts,
                });
            }
            Err(e) => e,
        };
        any_child_exited_early |= last.child_exited_early;
        if !last.retryable || attempts >= MAX_ATTEMPTS {
            return Err(RoundError {
                last,
                any_child_exited_early,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::ffi::OsStr;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use tokio::io::AsyncBufReadExt;
    use tokio::net::UnixStream;
    use tokio::time::Instant;

    use super::*;
    use crate::daemon::addr::tests::base_config;
    use crate::daemon::handshake::{Hello, RefusalKind};

    fn resolved(key: &str) -> Resolved {
        let run_dir = PathBuf::from("/run");
        Resolved {
            config: base_config(),
            paths: DaemonPaths {
                socket: run_dir.join(format!("{key}.sock")),
                pid_file: run_dir.join(format!("{key}.pid")),
                lock_file: run_dir.join(format!("{key}.lock")),
                log_file: run_dir.join("ferrexd.log"),
                key: key.to_owned(),
                run_dir,
            },
        }
    }

    fn failure(kind: Option<RefusalKind>, retryable: bool) -> AttemptError {
        AttemptError {
            kind,
            retryable,
            child_exited_early: false,
            message: "boom".into(),
        }
    }

    #[test]
    fn log_hint_names_log_file() {
        let hint = log_hint(Path::new("/run/ferrexd.log"), None);
        assert!(hint.contains("/run/ferrexd.log"), "{hint}");
    }

    #[test]
    fn log_hint_includes_env_log_file() {
        let hint = log_hint(
            Path::new("/run/ferrexd.log"),
            Some(OsStr::new("/var/log/ferrex.log")),
        );
        assert!(hint.contains("/run/ferrexd.log"), "{hint}");
        assert!(hint.contains("/var/log/ferrex.log"), "{hint}");
    }

    #[tokio::test(start_paused = true)]
    async fn run_round_reresolves_key_between_attempts() {
        let keys = Cell::new(0);
        let result = run_round(
            || {
                keys.set(keys.get() + 1);
                Ok(resolved(if keys.get() == 1 { "a" } else { "b" }))
            },
            |r| async move {
                if r.paths.key == "a" {
                    Err(failure(Some(RefusalKind::Key), true))
                } else {
                    Ok(())
                }
            },
            0,
        )
        .await
        .unwrap();
        assert_eq!(result.attempts_used, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn run_round_retries_resolve_error() {
        let calls = Cell::new(0);
        let result = run_round(
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    Err(failure(None, false))
                } else {
                    Ok(resolved("k"))
                }
            },
            |_| async { Ok(()) },
            0,
        )
        .await
        .unwrap();
        assert_eq!(result.attempts_used, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn run_round_stops_after_three_attempts_with_backoff() {
        let start = Instant::now();
        let calls = Cell::new(0);
        let err = run_round(
            || Ok(resolved("k")),
            |_| {
                calls.set(calls.get() + 1);
                async { Err::<(), _>(failure(None, true)) }
            },
            0,
        )
        .await
        .unwrap_err();
        assert_eq!(calls.get(), 3);
        assert!(start.elapsed() >= Duration::from_millis(1500));
        assert!(err.last.retryable);
    }

    #[tokio::test(start_paused = true)]
    async fn run_round_ends_on_non_retryable() {
        let calls = Cell::new(0);
        let err = run_round(
            || Ok(resolved("k")),
            |_| {
                calls.set(calls.get() + 1);
                async { Err::<(), _>(failure(Some(RefusalKind::Init), false)) }
            },
            0,
        )
        .await
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert_eq!(err.last.kind, Some(RefusalKind::Init));
    }

    #[tokio::test(start_paused = true)]
    async fn run_round_tracks_child_exited_early() {
        let calls = Cell::new(0);
        let err = run_round(
            || Ok(resolved("k")),
            |_| {
                calls.set(calls.get() + 1);
                let mut e = failure(None, true);
                e.child_exited_early = calls.get() == 1;
                async { Err::<(), _>(e) }
            },
            0,
        )
        .await
        .unwrap_err();
        assert!(err.any_child_exited_early);
        assert!(!err.last.child_exited_early);
    }

    #[tokio::test(start_paused = true)]
    async fn run_round_honours_used() {
        let start = Instant::now();
        let first_call = Cell::new(None);
        let calls = Cell::new(0);
        run_round(
            || Ok(resolved("k")),
            |_| {
                calls.set(calls.get() + 1);
                first_call.set(first_call.get().or_else(|| Some(start.elapsed())));
                async { Err::<(), _>(failure(None, true)) }
            },
            2,
        )
        .await
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert!(first_call.get().unwrap() >= Duration::from_secs(1));

        let start = Instant::now();
        let first_call = Cell::new(None);
        let ok = run_round(
            || Ok(resolved("k")),
            |_| {
                first_call.set(first_call.get().or_else(|| Some(start.elapsed())));
                async { Ok(()) }
            },
            1,
        )
        .await
        .unwrap();
        assert_eq!(ok.attempts_used, 2);
        assert!(first_call.get().unwrap() >= Duration::from_millis(500));
    }

    #[test]
    fn classify_connect_error_maps_kinds() {
        let class = |kind: io::ErrorKind| classify_connect_error(&io::Error::from(kind));
        assert_eq!(class(io::ErrorKind::NotFound), ConnectClass::Stale);
        assert_eq!(class(io::ErrorKind::ConnectionRefused), ConnectClass::Stale);
        assert_eq!(class(io::ErrorKind::WouldBlock), ConnectClass::Busy);
        assert_eq!(class(io::ErrorKind::Interrupted), ConnectClass::Busy);
        assert_eq!(class(io::ErrorKind::TimedOut), ConnectClass::Busy);
        assert_eq!(class(io::ErrorKind::PermissionDenied), ConnectClass::Other);
        let notsock = io::Error::from_raw_os_error(rustix::io::Errno::NOTSOCK.raw_os_error());
        assert_eq!(classify_connect_error(&notsock), ConnectClass::Stale);
    }

    #[test]
    fn budgets_from_env_test_override() {
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("FERREX_TEST_PROXY_TIMEOUT_MS", "250") };
        let budgets = Budgets::from_env();
        let ms = Duration::from_millis(250);
        assert_eq!(
            [budgets.flock, budgets.accept, budgets.hello, budgets.replay],
            [ms; 4]
        );
    }

    #[test]
    fn budgets_default_without_override() {
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::remove_var("FERREX_TEST_PROXY_TIMEOUT_MS") };
        let budgets = Budgets::from_env();
        assert_eq!(budgets.flock, Duration::from_secs(300));
        assert_eq!(budgets.accept, Duration::from_secs(30));
        assert_eq!(budgets.hello, Duration::from_secs(3600));
        assert_eq!(budgets.replay, Duration::from_secs(30));
    }

    fn hello() -> Hello {
        Hello::new("k".into(), "default".into())
    }

    #[tokio::test(start_paused = true)]
    async fn send_hello_timeout_is_not_retryable() {
        let (ours, _silent) = UnixStream::pair().unwrap();
        let err = send_hello(ours, &hello(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(!err.retryable, "{err:?}");
        assert_eq!(err.kind, None);
    }

    #[tokio::test(start_paused = true)]
    async fn send_hello_eof_is_retryable() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let mut line = String::new();
            let _ = tokio::io::BufReader::new(theirs).read_line(&mut line).await;
        });
        let err = send_hello(ours, &hello(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.retryable, "{err:?}");
    }

    #[tokio::test]
    async fn send_hello_partial_reply_at_eof_is_retryable() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let (read, mut write) = theirs.into_split();
            let mut line = String::new();
            tokio::io::BufReader::new(read)
                .read_line(&mut line)
                .await
                .unwrap();
            let full = HelloReply::ok().encode();
            tokio::io::AsyncWriteExt::write_all(&mut write, &full.as_bytes()[..full.len() / 2])
                .await
                .unwrap();
        });
        let err = send_hello(ours, &hello(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.retryable, "{err:?}");
        assert_eq!(err.kind, None);
    }

    async fn reply_with(reply: HelloReply) -> Result<DaemonConn, AttemptError> {
        let (ours, theirs) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let (read, mut write) = theirs.into_split();
            let mut line = String::new();
            tokio::io::BufReader::new(read)
                .read_line(&mut line)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut write, reply.encode().as_bytes())
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        send_hello(ours, &hello(), Duration::from_secs(5)).await
    }

    #[tokio::test]
    async fn send_hello_ok_returns_conn() {
        let conn = reply_with(HelloReply::ok()).await.unwrap();
        assert_eq!(conn.key, "k");
    }

    #[tokio::test]
    async fn send_hello_refusals_classified() {
        for (kind, retryable) in [
            (RefusalKind::Protocol, false),
            (RefusalKind::Init, false),
            (RefusalKind::Key, true),
            (RefusalKind::Session, true),
        ] {
            let err = reply_with(HelloReply::refuse(kind, "nope"))
                .await
                .unwrap_err();
            assert_eq!(err.kind, Some(kind));
            assert_eq!(err.retryable, retryable, "{kind:?}");
            assert!(err.message.contains("nope"), "{}", err.message);
        }
    }

    fn temp_paths(dir: &Path) -> DaemonPaths {
        let mut paths = resolved("k").paths;
        paths.run_dir = dir.to_owned();
        paths.socket = dir.join("k.sock");
        paths.pid_file = dir.join("k.pid");
        paths.lock_file = dir.join("k.lock");
        paths.log_file = dir.join("ferrexd.log");
        paths
    }

    fn short_budgets() -> Budgets {
        let ms = Duration::from_millis(300);
        Budgets {
            flock: ms,
            accept: ms,
            hello: ms,
            replay: ms,
        }
    }

    fn sh(script: &str) -> (PathBuf, Vec<std::ffi::OsString>) {
        (PathBuf::from("/bin/sh"), vec!["-c".into(), script.into()])
    }

    #[tokio::test]
    async fn connect_or_spawn_uses_live_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = temp_paths(tmp.path());
        let _listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        let (exe, argv) = sh("exit 1");
        connect_or_spawn(&paths, &exe, &argv, &short_budgets())
            .await
            .unwrap();
        assert!(!paths.lock_file.exists());
    }

    #[tokio::test]
    async fn connect_or_spawn_child_exit_is_retryable_and_names_log() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = temp_paths(tmp.path());
        let (exe, argv) = sh("echo \"args: $1 $2\" >&2; exit 3");
        let mut budgets = short_budgets();
        budgets.accept = Duration::from_secs(30);
        let err = connect_or_spawn(&paths, &exe, &argv, &budgets)
            .await
            .unwrap_err();
        assert!(err.retryable && err.child_exited_early, "{err:?}");
        assert!(err.message.contains("ferrexd.log"), "{}", err.message);
        let log = std::fs::read_to_string(&paths.log_file).unwrap();
        assert_eq!(log, "args: --key k\n");
    }

    #[tokio::test]
    async fn connect_or_spawn_removes_stale_socket_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = temp_paths(tmp.path());
        std::fs::write(&paths.socket, "stale").unwrap();
        let (exe, argv) = sh("exit 1");
        let err = connect_or_spawn(&paths, &exe, &argv, &short_budgets())
            .await
            .unwrap_err();
        assert!(err.child_exited_early, "{err:?}");
        assert!(!paths.socket.exists());
    }

    #[tokio::test]
    async fn connect_or_spawn_accepts_once_socket_binds() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = temp_paths(tmp.path());
        let socket = paths.socket.clone();
        let binder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            tokio::net::UnixListener::bind(socket).unwrap()
        });
        let (exe, argv) = sh("sleep 5");
        let mut budgets = short_budgets();
        budgets.accept = Duration::from_secs(5);
        connect_or_spawn(&paths, &exe, &argv, &budgets)
            .await
            .unwrap();
        let _listener = binder.await.unwrap();
        let lock = std::fs::File::options()
            .write(true)
            .open(&paths.lock_file)
            .unwrap();
        lock.try_lock().unwrap();
    }

    #[tokio::test]
    async fn connect_or_spawn_accept_timeout_is_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = temp_paths(tmp.path());
        let (exe, argv) = sh("sleep 30");
        let err = connect_or_spawn(&paths, &exe, &argv, &short_budgets())
            .await
            .unwrap_err();
        assert!(err.retryable && !err.child_exited_early, "{err:?}");
        assert!(err.message.contains("ferrexd.log"), "{}", err.message);
    }

    #[tokio::test]
    async fn connect_or_spawn_lock_timeout_is_not_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = temp_paths(tmp.path());
        let held = std::fs::File::create(&paths.lock_file).unwrap();
        held.lock().unwrap();
        let (exe, argv) = sh("exit 1");
        let err = connect_or_spawn(&paths, &exe, &argv, &short_budgets())
            .await
            .unwrap_err();
        assert!(!err.retryable, "{err:?}");
        assert!(
            err.message.contains(paths.lock_file.to_str().unwrap()),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn connect_or_spawn_unusable_run_dir_is_not_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, "").unwrap();
        let paths = temp_paths(&file);
        let (exe, argv) = sh("exit 1");
        let err = connect_or_spawn(&paths, &exe, &argv, &short_budgets())
            .await
            .unwrap_err();
        assert!(!err.retryable, "{err:?}");
        assert!(
            err.message.contains(file.to_str().unwrap()),
            "{}",
            err.message
        );
    }
}
