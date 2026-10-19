mod common;

use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use ferrex_store::{MetadataStore, SqliteStore};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::time::Instant;

use common::{
    CmdOpts, DaemonGuard, PathsJson, TestEnv, raw_session, read_pid_file, sigterm, wait_exit,
    wait_for_path,
};

const COLD_HELLO: Duration = Duration::from_secs(60);
const BIND_WITHIN: Duration = Duration::from_secs(10);
/// Mirrors the daemon's `SHUTDOWN_LOCK_WAIT`.
const SHUTDOWN_LOCK_WAIT: Duration = Duration::from_secs(10);
const SHUTDOWN_SLACK: Duration = Duration::from_secs(8);

struct Daemon {
    child: Child,
    paths: PathsJson,
    guard: DaemonGuard,
}

impl Daemon {
    async fn start(env: &TestEnv, vars: &[(&str, &str)]) -> Self {
        let paths = env.paths().await;
        let child = spawn_daemon(env, &paths.key, vars);
        let guard = DaemonGuard::for_child(&child);
        wait_for_path(&paths.pid_file, BIND_WITHIN).await;
        Self {
            child,
            paths,
            guard,
        }
    }

    async fn session(&self, ns: &str, hello_timeout: Duration) -> common::McpConn {
        let (reply, conn) =
            raw_session(&self.paths.socket, &self.paths.key, ns, hello_timeout).await;
        assert!(reply.ok, "hello refused: {reply:?}");
        let mut conn = conn.expect("ok hello yields a session");
        conn.initialize().await;
        conn
    }

    async fn wait_exit(&mut self, within: Duration) -> ExitStatus {
        let status = wait_exit(&mut self.child, within).await;
        self.guard.disarm();
        status
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_none()
    }
}

fn spawn_daemon(env: &TestEnv, key: &str, vars: &[(&str, &str)]) -> Child {
    env.ferrex_cmd_with(CmdOpts {
        extra_args: vec!["daemon".into(), "--key".into(), key.into()],
        env: vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        ..Default::default()
    })
    .spawn()
    .expect("spawn daemon")
}

fn open_lock(path: &Path) -> File {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .expect("open lock file")
}

fn hold_lock(path: &Path) -> File {
    let file = open_lock(path);
    file.lock().expect("take spawn lock");
    file
}

fn inode(path: &Path) -> u64 {
    std::fs::symlink_metadata(path).expect("stat").ino()
}

fn tool_text(resp: &Value) -> &str {
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool result has no text: {resp}"))
}

async fn sleep_until(t: Instant) {
    tokio::time::sleep_until(t).await;
}

#[tokio::test]
async fn two_clients_served_concurrently() {
    let env = TestEnv::new().await;
    let daemon = Daemon::start(&env, &[]).await;
    let (mut a, mut b) = tokio::join!(
        daemon.session("default", COLD_HELLO),
        daemon.session("default", COLD_HELLO)
    );
    let (ta, tb) = tokio::join!(a.tool_names(), b.tool_names());
    assert!(ta.iter().any(|t| t == "recall"), "{ta:?}");
    assert_eq!(ta, tb);
}

#[tokio::test]
async fn recall_in_fresh_namespace_is_empty() {
    let env = TestEnv::new().await;
    let daemon = Daemon::start(&env, &[]).await;
    let ns = format!("fresh_{}", uuid::Uuid::now_v7().simple());
    let mut conn = daemon.session(&ns, COLD_HELLO).await;
    let resp = conn
        .request(
            "tools/call",
            json!({"name": "recall", "arguments": {"query": "anything", "namespace": ns}}),
        )
        .await;
    let results: Value = serde_json::from_str(tool_text(&resp)).expect("recall json");
    assert_eq!(results, json!([]), "{resp}");
}

#[tokio::test]
async fn init_failure_refuses_then_exits() {
    let env = TestEnv::new().await;
    {
        let store = SqliteStore::open_with_pool_size(&env.db_path, 1).expect("open db");
        store
            .set_metadata("embedding_model", "bogus")
            .await
            .expect("seed model");
    }
    let mut daemon = Daemon::start(&env, &[("FERREX_TEST_INIT_FAIL_GRACE_MS", "1500")]).await;
    let (reply, conn) = raw_session(
        &daemon.paths.socket,
        &daemon.paths.key,
        "default",
        COLD_HELLO,
    )
    .await;
    assert!(conn.is_none());
    assert_eq!(reply.kind.as_deref(), Some("init"), "{reply:?}");
    let error = reply.error.unwrap_or_default();
    assert!(error.contains("embedding model mismatch"), "{error}");
    let status = daemon.wait_exit(Duration::from_secs(5)).await;
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn bogus_key_exits_before_binding() {
    let env = TestEnv::without_sidecar();
    let paths = env.paths().await;
    let mut child = spawn_daemon(&env, "bogus", &[]);
    let mut guard = DaemonGuard::for_child(&child);
    let status = wait_exit(&mut child, Duration::from_secs(10)).await;
    guard.disarm();
    assert!(!status.success());
    assert!(!paths.socket.exists());
}

#[tokio::test]
async fn idle_exit_with_no_connections() {
    let env = TestEnv::new().await;
    let mut daemon = Daemon::start(&env, &[("FERREX_IDLE_TIMEOUT", "2")]).await;
    let status = daemon.wait_exit(Duration::from_secs(60)).await;
    assert!(status.success(), "{status}");
    assert!(!daemon.paths.socket.exists());
    assert!(!daemon.paths.pid_file.exists());
}

#[tokio::test]
async fn connection_during_idle_pause_is_served() {
    let env = TestEnv::new().await;
    let mut daemon = Daemon::start(
        &env,
        &[
            ("FERREX_IDLE_TIMEOUT", "2"),
            ("FERREX_TEST_IDLE_PAUSE_MS", "3000"),
        ],
    )
    .await;
    let lock = open_lock(&daemon.paths.lock_file);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match lock.try_lock() {
            Ok(()) => lock.unlock().expect("unlock"),
            Err(TryLockError::WouldBlock) => break,
            Err(TryLockError::Error(e)) => panic!("try_lock: {e}"),
        }
        assert!(
            Instant::now() < deadline,
            "daemon never took the spawn lock for its idle exit"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut conn = daemon.session("default", Duration::from_secs(20)).await;
    assert!(!conn.tool_names().await.is_empty());
    assert!(
        daemon.alive(),
        "daemon exited despite a connection in the pause"
    );
}

#[tokio::test]
async fn admin_guard_probes_do_not_delay_idle_exit() {
    let env = TestEnv::new().await;
    let mut daemon = Daemon::start(&env, &[("FERREX_IDLE_TIMEOUT", "2")]).await;
    let (reply, conn) = raw_session(
        &daemon.paths.socket,
        &daemon.paths.key,
        "default",
        COLD_HELLO,
    )
    .await;
    assert!(reply.ok, "{reply:?}");
    drop(conn);

    let deadline = Instant::now() + Duration::from_secs(2 + 15);
    let mut refusals = 0;
    while daemon.alive() {
        assert!(
            Instant::now() < deadline,
            "daemon outlived its idle timeout while being probed"
        );
        let out = env
            .ferrex_cmd_with(CmdOpts {
                extra_args: ["audit", "reconcile", "--fix"].map(Into::into).into(),
                ..Default::default()
            })
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .expect("run audit");
        if String::from_utf8_lossy(&out.stderr).contains("Close your Claude sessions") {
            refusals += 1;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(refusals > 0, "the guard never saw the daemon");
    let status = daemon.wait_exit(Duration::from_secs(5)).await;
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn bad_or_silent_hello_does_not_block_idle_exit() {
    let env = TestEnv::new().await;
    let mut daemon = Daemon::start(&env, &[("FERREX_IDLE_TIMEOUT", "2")]).await;
    let (reply, conn) = raw_session(
        &daemon.paths.socket,
        &daemon.paths.key,
        "default",
        COLD_HELLO,
    )
    .await;
    let t0 = Instant::now();
    assert!(reply.ok, "{reply:?}");
    drop(conn);

    let mut garbage = UnixStream::connect(&daemon.paths.socket)
        .await
        .expect("connect");
    garbage
        .write_all(b"this is not a hello\n")
        .await
        .expect("write garbage");
    let _silent = UnixStream::connect(&daemon.paths.socket)
        .await
        .expect("connect");

    let budget = Duration::from_secs(2 + 10 + 8);
    let remaining = budget.saturating_sub(t0.elapsed());
    let status = daemon.wait_exit(remaining).await;
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn invalid_namespace_gets_session_refusal_others_fine() {
    let env = TestEnv::new().await;
    let daemon = Daemon::start(&env, &[]).await;
    let mut good = daemon.session("default", COLD_HELLO).await;
    let (reply, conn) = raw_session(
        &daemon.paths.socket,
        &daemon.paths.key,
        "bad/ns",
        COLD_HELLO,
    )
    .await;
    assert!(conn.is_none());
    assert_eq!(reply.kind.as_deref(), Some("session"), "{reply:?}");
    assert!(
        reply
            .error
            .as_deref()
            .is_some_and(|e| e.contains("invalid namespace")),
        "{reply:?}"
    );
    assert!(!good.tool_names().await.is_empty());
}

#[tokio::test]
async fn second_daemon_for_live_key_exits_zero() {
    let env = TestEnv::new().await;
    let daemon = Daemon::start(&env, &[("FERREX_IDLE_TIMEOUT", "600")]).await;
    let ino = inode(&daemon.paths.socket);
    let pid_before = std::fs::read(&daemon.paths.pid_file).expect("read pid file");

    let mut second = spawn_daemon(&env, &daemon.paths.key, &[("FERREX_IDLE_TIMEOUT", "600")]);
    let mut second_guard = DaemonGuard::for_child(&second);
    let status = wait_exit(&mut second, Duration::from_secs(30)).await;
    second_guard.disarm();
    assert!(status.success(), "{status}");
    assert_eq!(inode(&daemon.paths.socket), ino);
    assert_eq!(
        std::fs::read(&daemon.paths.pid_file).expect("read pid file"),
        pid_before
    );
}

#[tokio::test]
async fn sigterm_with_lock_held_leaves_files() {
    let env = TestEnv::new().await;
    let mut daemon = Daemon::start(&env, &[("FERREX_IDLE_TIMEOUT", "600")]).await;
    let _lock = hold_lock(&daemon.paths.lock_file);
    sigterm(&daemon.child);
    let status = daemon.wait_exit(SHUTDOWN_LOCK_WAIT + SHUTDOWN_SLACK).await;
    assert!(status.success(), "{status}");
    assert!(daemon.paths.socket.exists());
    assert!(daemon.paths.pid_file.exists());
}

#[tokio::test]
async fn idle_exit_with_lock_held_keeps_serving() {
    let env = TestEnv::new().await;
    let mut daemon = Daemon::start(&env, &[("FERREX_IDLE_TIMEOUT", "2")]).await;
    let _lock = hold_lock(&daemon.paths.lock_file);
    let (reply, conn) = raw_session(
        &daemon.paths.socket,
        &daemon.paths.key,
        "default",
        COLD_HELLO,
    )
    .await;
    assert!(reply.ok, "{reply:?}");
    drop(conn);
    let t0 = Instant::now();

    sleep_until(t0 + Duration::from_secs(15)).await;
    assert!(
        daemon.alive(),
        "daemon exited while the spawn lock was held"
    );
    let (reply, _conn) = raw_session(
        &daemon.paths.socket,
        &daemon.paths.key,
        "default",
        Duration::from_secs(20),
    )
    .await;
    assert!(reply.ok, "{reply:?}");
}

#[tokio::test]
async fn idle_exit_rearms_after_lock_released() {
    let env = TestEnv::new().await;
    let mut daemon = Daemon::start(&env, &[("FERREX_IDLE_TIMEOUT", "2")]).await;
    let lock = hold_lock(&daemon.paths.lock_file);
    let (reply, conn) = raw_session(
        &daemon.paths.socket,
        &daemon.paths.key,
        "default",
        COLD_HELLO,
    )
    .await;
    assert!(reply.ok, "{reply:?}");
    drop(conn);
    let t0 = Instant::now();

    sleep_until(t0 + Duration::from_secs(2 + 10 + 2)).await;
    assert!(
        daemon.alive(),
        "daemon exited while the spawn lock was held"
    );
    drop(lock);
    let status = daemon.wait_exit(Duration::from_secs(2 + 10 + 8)).await;
    assert!(status.success(), "{status}");
    assert!(!daemon.paths.socket.exists());
    assert!(!daemon.paths.pid_file.exists());
}

#[tokio::test]
async fn init_timeout_reports_timeout() {
    let env = TestEnv::new().await;
    let started = Instant::now();
    let daemon = Daemon::start(
        &env,
        &[
            ("FERREX_TEST_INIT_DELAY_MS", "5000"),
            ("FERREX_INIT_TIMEOUT", "1"),
            ("FERREX_TEST_INIT_FAIL_GRACE_MS", "10000"),
        ],
    )
    .await;
    let hello = Duration::from_secs(20);
    let (reply, _) = raw_session(&daemon.paths.socket, &daemon.paths.key, "default", hello).await;
    assert_eq!(reply.kind.as_deref(), Some("init"), "{reply:?}");
    let error = reply.error.unwrap_or_default();
    assert!(error.contains("timed out"), "{error}");

    sleep_until(started + Duration::from_secs(7)).await;
    let (reply, _) = raw_session(&daemon.paths.socket, &daemon.paths.key, "default", hello).await;
    assert_eq!(reply.kind.as_deref(), Some("init"), "{reply:?}");
}

#[tokio::test]
async fn shutdown_never_deletes_foreign_files() {
    let env = TestEnv::new().await;
    let mut daemon = Daemon::start(&env, &[("FERREX_IDLE_TIMEOUT", "600")]).await;
    let foreign = json!({"pid": 1, "db_path": env.db_path}).to_string();
    std::fs::write(&daemon.paths.pid_file, &foreign).expect("replace pid file");
    sigterm(&daemon.child);
    let status = daemon.wait_exit(Duration::from_secs(12)).await;
    assert!(status.success(), "{status}");
    assert_eq!(
        std::fs::read_to_string(&daemon.paths.pid_file).expect("pid file kept"),
        foreign
    );
}

#[tokio::test]
async fn shutdown_never_deletes_foreign_socket() {
    let env = TestEnv::new().await;
    let mut daemon = Daemon::start(&env, &[("FERREX_IDLE_TIMEOUT", "600")]).await;
    std::fs::remove_file(&daemon.paths.socket).expect("unlink socket");
    let _foreign =
        std::os::unix::net::UnixListener::bind(&daemon.paths.socket).expect("bind foreign");
    let ino = inode(&daemon.paths.socket);
    sigterm(&daemon.child);
    let status = daemon.wait_exit(Duration::from_secs(12)).await;
    assert!(status.success(), "{status}");
    assert_eq!(inode(&daemon.paths.socket), ino);
}

#[tokio::test]
async fn pid_file_holds_pid_and_db_path() {
    let env = TestEnv::new().await;
    let daemon = Daemon::start(&env, &[("FERREX_IDLE_TIMEOUT", "600")]).await;
    let pid = read_pid_file(&daemon.paths.pid_file).expect("parse pid file");
    assert_eq!(Some(pid.pid), daemon.child.id());
    assert_eq!(pid.db_path, env.db_path);
}
