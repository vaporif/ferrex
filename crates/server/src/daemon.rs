pub mod addr;
mod env;
pub mod handshake;
mod idle;
mod session;
pub mod spawn;

use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use eyre::WrapErr;
use ferrex_core::{FerrexConfig, MemoryService, SidecarPolicy};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::daemon::addr::{
    DaemonPaths, PidFile, current_exe_canonical, ensure_run_dir, try_lock_until,
};
use crate::daemon::env::{env_duration, test_hook_millis};
use crate::daemon::idle::{ConnEvent, IdleTimer};
use crate::daemon::session::{InitView, SessionCtx, serve_connection};

const INIT_TIMEOUT_DEFAULT_SECS: u64 = 1800;
const INIT_TIMEOUT_CAP_SECS: u64 = 3000;
const IDLE_TIMEOUT_DEFAULT_SECS: u64 = 600;
const INIT_FAIL_GRACE: Duration = Duration::from_secs(60);
const SHUTDOWN_LOCK_WAIT: Duration = Duration::from_secs(10);
const SERVICE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

type InitResult = Result<MemoryService, String>;

/// Runs the shared daemon for `key`; returns only on errors before the socket is bound.
pub fn run(mut config: FerrexConfig, key: &str) -> eyre::Result<()> {
    if std::env::var_os("FERREX_TEST_DAEMON_EXIT_BEFORE_BIND").is_some() {
        eyre::bail!("FERREX_TEST_DAEMON_EXIT_BEFORE_BIND is set, exiting before bind");
    }
    let exe = current_exe_canonical()?;
    let paths = DaemonPaths::for_config(&config, &exe)?;
    if paths.key != key {
        eyre::bail!(
            "daemon key mismatch: started with --key {key}, but this binary and config give {}",
            paths.key
        );
    }
    // The key ignores namespace and every session sends its own. Pinning it keeps the daemon
    // independent of whichever proxy spawned it.
    config.namespace = "default".into();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        Daemon::bind(config, paths).await?.serve().await;
        eyre::Ok(())
    })?;
    exit_skipping_atexit(0)
}

/// Exits without C/C++ exit handlers: onnxruntime's static destructors crash a model load or
/// inference still running on the abandoned init thread or a blocking-pool thread.
fn exit_skipping_atexit(code: i32) -> ! {
    use std::io::Write;

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // `_exit` skips the atexit hook that writes coverage data, so write it here.
    #[cfg(coverage)]
    let _ = posix::write_coverage_profile();
    posix::exit_immediately(code)
}

mod posix {
    // SAFETY: `_exit` has this exact C signature on every Unix, takes any status and has no
    // preconditions, so calling it is always sound.
    unsafe extern "C" {
        #[link_name = "_exit"]
        pub safe fn exit_immediately(status: std::ffi::c_int) -> !;

        // SAFETY: the LLVM profiler runtime linked into `-C instrument-coverage` builds defines
        // it with this signature; it only writes the profile file.
        #[cfg(coverage)]
        #[link_name = "__llvm_profile_write_file"]
        pub safe fn write_coverage_profile() -> std::ffi::c_int;
    }
}

enum InitState {
    Pending(oneshot::Receiver<InitResult>),
    Ready(Arc<MemoryService>),
    Failed(String),
}

impl InitState {
    const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }

    async fn result(&mut self) -> Result<InitResult, oneshot::error::RecvError> {
        match self {
            Self::Pending(rx) => rx.await,
            Self::Ready(_) | Self::Failed(_) => std::future::pending().await,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketId {
    dev: u64,
    ino: u64,
}

impl From<&Metadata> for SocketId {
    fn from(meta: &Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Idle,
    Signal(&'static str),
    InitFailedGrace,
}

/// A shutdown past the point of no return; `lock` is `None` when the spawn lock wasn't free.
struct Commit {
    lock: Option<File>,
}

struct Daemon {
    paths: DaemonPaths,
    listener: UnixListener,
    socket_id: SocketId,
    /// Held until exit: admin commands and proxies read it as "this daemon is alive".
    _pid_lock: File,
    sigterm: Signal,
    sigint: Signal,
    init: InitState,
    init_view: watch::Sender<InitView>,
    init_timeout: Duration,
    init_deadline: Instant,
    grace_deadline: Option<Instant>,
    idle: IdleTimer,
    sessions: JoinSet<()>,
    events_tx: mpsc::UnboundedSender<ConnEvent>,
    events_rx: mpsc::UnboundedReceiver<ConnEvent>,
}

impl Daemon {
    async fn bind(config: FerrexConfig, paths: DaemonPaths) -> eyre::Result<Self> {
        ensure_run_dir(&paths.run_dir)?;
        let sigterm = signal(SignalKind::terminate())?;
        let sigint = signal(SignalKind::interrupt())?;
        if UnixStream::connect(&paths.socket).await.is_ok() {
            tracing::info!(socket = %paths.socket.display(), "another daemon already serves this key");
            std::process::exit(0);
        }
        if let Some(delay) = test_hook_millis("FERREX_TEST_DAEMON_BIND_DELAY_MS") {
            tokio::time::sleep(delay).await;
        }
        let listener = bind_listener(&paths.socket).await?;
        let socket_id = std::fs::symlink_metadata(&paths.socket)
            .map(|meta| SocketId::from(&meta))
            .wrap_err_with(|| format!("stat bound socket {}", paths.socket.display()))?;
        let pid_file = PidFile {
            pid: std::process::id(),
            db_path: config.db_path.clone(),
        };
        let pid_lock = match pid_file.write_locked(&paths.pid_file) {
            Ok(lock) => lock,
            Err(e) => {
                remove_socket_if_ours(&paths.socket, socket_id);
                return Err(e)
                    .wrap_err_with(|| format!("write pid file {}", paths.pid_file.display()));
            }
        };
        tracing::info!(key = %paths.key, socket = %paths.socket.display(), "daemon listening");

        let init_timeout = env_duration(
            "FERREX_INIT_TIMEOUT",
            INIT_TIMEOUT_DEFAULT_SECS,
            Some(INIT_TIMEOUT_CAP_SECS),
        );
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (init_view, _) = watch::channel(InitView::Pending);
        Ok(Self {
            paths,
            listener,
            socket_id,
            _pid_lock: pid_lock,
            sigterm,
            sigint,
            init: InitState::Pending(start_init(config)),
            init_view,
            init_timeout,
            init_deadline: Instant::now() + init_timeout,
            grace_deadline: None,
            idle: IdleTimer::new(env_duration(
                "FERREX_IDLE_TIMEOUT",
                IDLE_TIMEOUT_DEFAULT_SECS,
                None,
            )),
            sessions: JoinSet::new(),
            events_tx,
            events_rx,
        })
    }

    async fn serve(mut self) {
        let commit = loop {
            self.idle.update(Instant::now());
            let trigger = tokio::select! {
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, _)) => self.spawn_session(stream),
                        Err(e) => {
                            tracing::warn!(error = %e, "accept failed");
                            tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        }
                    }
                    continue;
                }
                _ = self.sigterm.recv() => Trigger::Signal("SIGTERM"),
                _ = self.sigint.recv() => Trigger::Signal("SIGINT"),
                result = self.init.result(), if self.init.is_pending() => {
                    self.on_init_result(result);
                    continue;
                }
                () = tokio::time::sleep_until(self.init_deadline), if self.init.is_pending() => {
                    self.fail_init(format!("init timed out after {}s", self.init_timeout.as_secs()));
                    continue;
                }
                () = sleep_until_opt(self.grace_deadline), if self.grace_deadline.is_some() => {
                    Trigger::InitFailedGrace
                }
                () = sleep_until_opt(self.idle.deadline()), if self.idle.deadline().is_some() => {
                    Trigger::Idle
                }
                Some(event) = self.events_rx.recv() => {
                    self.idle.apply(event);
                    continue;
                }
                Some(joined) = self.sessions.join_next() => {
                    if let Err(e) = joined
                        && e.is_panic()
                    {
                        tracing::error!(error = %e, "session task panicked");
                    }
                    continue;
                }
            };
            tracing::info!(?trigger, "shutting down");
            if let Some(commit) = self.begin_shutdown(trigger).await {
                break commit;
            }
        };
        self.finish(commit).await;
    }

    fn spawn_session(&mut self, stream: UnixStream) {
        self.idle.accepted();
        let ctx = SessionCtx {
            key: Arc::from(self.paths.key.as_str()),
            init: self.init_view.subscribe(),
            events: self.events_tx.clone(),
        };
        self.sessions.spawn(serve_connection(stream, ctx));
    }

    fn on_init_result(&mut self, result: Result<InitResult, oneshot::error::RecvError>) {
        match result {
            Ok(Ok(service)) => {
                let service = Arc::new(service);
                self.init = InitState::Ready(Arc::clone(&service));
                self.init_view.send_replace(InitView::Ready(service));
                self.idle.init_succeeded();
                tracing::info!("ferrex daemon ready");
            }
            Ok(Err(e)) => self.fail_init(e),
            Err(_) => self.fail_init("init thread exited without a result".into()),
        }
    }

    fn fail_init(&mut self, error: String) {
        let grace = test_hook_millis("FERREX_TEST_INIT_FAIL_GRACE_MS").unwrap_or(INIT_FAIL_GRACE);
        tracing::error!(%error, ?grace, "init failed, refusing hellos until shutdown");
        self.init_view
            .send_replace(InitView::Failed(Arc::from(error.as_str())));
        self.init = InitState::Failed(error);
        self.grace_deadline = Some(Instant::now() + grace);
    }

    /// Takes the spawn lock; for an idle exit, re-checks for late connections.
    /// `None` means keep serving.
    async fn begin_shutdown(&mut self, trigger: Trigger) -> Option<Commit> {
        let lock = acquire_lock_within(&self.paths.lock_file, SHUTDOWN_LOCK_WAIT).await;
        if trigger != Trigger::Idle {
            if lock.is_some() {
                drop(self.drain_backlog());
            } else {
                tracing::warn!("spawn lock busy, leaving socket and pid file in place");
            }
            return Some(Commit { lock });
        }
        let Some(lock) = lock else {
            tracing::info!("spawn lock busy, postponing idle exit");
            self.idle.postpone(Instant::now());
            return None;
        };
        if let Some(pause) = test_hook_millis("FERREX_TEST_IDLE_PAUSE_MS") {
            tokio::time::sleep(pause).await;
        }
        let drained = self.drain_backlog();
        while let Ok(event) = self.events_rx.try_recv() {
            self.idle.apply(event);
        }
        if drained.is_empty() && self.idle.should_arm() {
            return Some(Commit { lock: Some(lock) });
        }
        tracing::info!(
            drained = drained.len(),
            "connection arrived, cancelling idle exit"
        );
        for stream in drained {
            match stream
                .set_nonblocking(true)
                .and_then(|()| UnixStream::from_std(stream))
            {
                Ok(stream) => self.spawn_session(stream),
                Err(e) => tracing::warn!(error = %e, "adopt drained connection"),
            }
        }
        release_lock(&lock);
        None
    }

    /// Accepts whatever is queued without waiting, through a duplicate of the listener fd.
    fn drain_backlog(&self) -> Vec<std::os::unix::net::UnixStream> {
        let listener = match self.listener.as_fd().try_clone_to_owned() {
            Ok(fd) => std::os::unix::net::UnixListener::from(fd),
            Err(e) => {
                tracing::warn!(error = %e, "duplicate listener fd for drain");
                return Vec::new();
            }
        };
        let mut drained = Vec::new();
        loop {
            match listener.accept() {
                Ok((stream, _)) => drained.push(stream),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    if e.kind() != io::ErrorKind::WouldBlock {
                        tracing::warn!(error = %e, "drain accept failed");
                    }
                    break drained;
                }
            }
        }
    }

    /// Removes socket and pid file if we hold the lock, then stops sessions and the service,
    /// which owns the sidecar.
    async fn finish(mut self, commit: Commit) {
        drop(self.listener);
        if let Some(lock) = commit.lock {
            remove_pid_file_if_ours(&self.paths.pid_file);
            remove_socket_if_ours(&self.paths.socket, self.socket_id);
            release_lock(&lock);
        }
        self.sessions.abort_all();
        match &self.init {
            InitState::Ready(service) => {
                if tokio::time::timeout(SERVICE_SHUTDOWN_TIMEOUT, service.shutdown())
                    .await
                    .is_err()
                {
                    tracing::warn!("service shutdown timed out after {SERVICE_SHUTDOWN_TIMEOUT:?}");
                }
            }
            InitState::Failed(error) => tracing::info!(%error, "exiting after failed init"),
            InitState::Pending(_) => {}
        }
        tracing::info!("daemon stopped");
    }
}

async fn bind_listener(socket: &Path) -> eyre::Result<UnixListener> {
    match UnixListener::bind(socket) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(socket).await.is_ok() {
                tracing::info!(socket = %socket.display(), "another daemon won the bind race");
                std::process::exit(0);
            }
            eyre::bail!("stale or busy socket {}, not unlinking", socket.display())
        }
        Err(e) => Err(e).wrap_err_with(|| format!("bind {}", socket.display())),
    }
}

fn start_init(config: FerrexConfig) -> oneshot::Receiver<InitResult> {
    let (tx, rx) = oneshot::channel();
    let handle = tokio::runtime::Handle::current();
    let spawned = std::thread::Builder::new()
        .name("ferrex-init".into())
        .spawn(move || {
            if let Some(delay) = test_hook_millis("FERREX_TEST_INIT_DELAY_MS") {
                std::thread::sleep(delay);
            }
            let result = handle
                .block_on(MemoryService::from_config_with(
                    config,
                    SidecarPolicy::LeaveRunning,
                ))
                .map_err(|e| e.to_string());
            // The accept loop drops the receiver once the init deadline has passed.
            let _ = tx.send(result);
        });
    // On failure the closure, and with it the sender, is dropped, so the loop sees init fail.
    if let Err(e) = spawned {
        tracing::error!(error = %e, "spawn init thread");
    }
    rx
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn acquire_lock_within(path: &Path, budget: Duration) -> Option<File> {
    try_lock_until(path, Instant::now() + budget)
        .await
        .inspect_err(|e| tracing::warn!(path = %path.display(), error = %e, "lock spawn lock"))
        .ok()
        .flatten()
}

fn release_lock(lock: &File) {
    if let Err(e) = lock.unlock() {
        tracing::warn!(error = %e, "unlock spawn lock");
    }
}

fn remove_pid_file_if_ours(path: &Path) {
    match PidFile::read(path) {
        Some(pid) if pid.pid == std::process::id() => remove_logged(path),
        _ => tracing::info!(path = %path.display(), "pid file is not ours, leaving it"),
    }
}

fn remove_socket_if_ours(path: &Path, ours: SocketId) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if SocketId::from(&meta) == ours => remove_logged(path),
        Ok(_) => tracing::info!(path = %path.display(), "socket was replaced, leaving it"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "stat socket"),
    }
}

fn remove_logged(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), error = %e, "remove file");
    }
}
