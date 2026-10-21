mod common;

use std::fs::{OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use common::{
    CmdOpts, DaemonGuard, FakeBehaviour, FakeConn, FakeDaemon, PathsJson, ProxyClient, TestEnv,
    read_pid_file, signal,
};
use pretty_assertions::assert_eq;

const PID_FILE_WITHIN: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

fn env_opts(vars: &[(&str, &str)]) -> CmdOpts {
    CmdOpts {
        env: vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        ..Default::default()
    }
}

async fn expect_exit(proxy: &mut ProxyClient, within: Duration) -> ExitStatus {
    proxy.wait_exit(within).await.unwrap_or_else(|| {
        panic!(
            "proxy did not exit within {within:?}; stderr:\n{}",
            proxy.stderr_text()
        )
    })
}

fn assert_initialized(resp: &Value) {
    assert!(
        resp["result"]["serverInfo"].is_object(),
        "initialize failed: {resp}"
    );
}

async fn tool_names(proxy: &mut ProxyClient) -> Vec<String> {
    let resp = proxy.request("tools/list", json!({})).await;
    resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result: {resp}"))
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect()
}

async fn call_tool(proxy: &mut ProxyClient, name: &str, arguments: Value) -> Value {
    let resp = proxy
        .request("tools/call", json!({"name": name, "arguments": arguments}))
        .await;
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{name} failed: {resp}"));
    serde_json::from_str(text).expect("tool result json")
}

fn run_dir_files(run_dir: &Path, ext: &str) -> Vec<PathBuf> {
    std::fs::read_dir(run_dir).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|x| x == ext))
                .collect()
        },
    )
}

fn lock_is_free(path: &Path) -> bool {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .unwrap();
    match file.try_lock() {
        Ok(()) => true,
        Err(TryLockError::WouldBlock) => false,
        Err(TryLockError::Error(e)) => panic!("try_lock {}: {e}", path.display()),
    }
}

async fn wait_until(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {within:?}");
        tokio::time::sleep(POLL).await;
    }
}

async fn daemon_guard(paths: &PathsJson) -> DaemonGuard {
    DaemonGuard::from_pid_file(&paths.pid_file, PID_FILE_WITHIN).await
}

#[tokio::test]
async fn initialize_tools_list_store_recall_through_proxy() {
    let env = TestEnv::new().await;
    let paths = env.paths().await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    assert_initialized(&proxy.initialize().await);
    let _guard = daemon_guard(&paths).await;
    assert!(tool_names(&mut proxy).await.contains(&"store".to_owned()));
    let stored = call_tool(
        &mut proxy,
        "store",
        json!({"content": "Deployed API v2.1 to production", "memory_type": "episodic"}),
    )
    .await;
    assert_eq!(stored["stored"], true, "{stored}");
    let recalled = call_tool(&mut proxy, "recall", json!({"query": "API deployment"})).await;
    let hits = recalled.as_array().expect("recall returns an array");
    assert!(hits.iter().any(|r| r["id"] == stored["id"]), "{recalled}");
}

#[tokio::test]
async fn no_daemon_env_selects_standalone() {
    let env = TestEnv::new().await;
    let mut proxy = ProxyClient::spawn(&env, env_opts(&[("FERREX_NO_DAEMON", "1")]));
    assert_initialized(&proxy.initialize().await);
    assert!(!tool_names(&mut proxy).await.is_empty());
    assert!(run_dir_files(&env.run_dir, "sock").is_empty());
    assert!(run_dir_files(&env.run_dir, "pid").is_empty());
}

#[tokio::test]
async fn second_proxy_reuses_daemon() {
    let env = TestEnv::new().await;
    let paths = env.paths().await;
    let mut first = ProxyClient::spawn(&env, CmdOpts::default());
    assert_initialized(&first.initialize().await);
    let guard = daemon_guard(&paths).await;
    let mut second = ProxyClient::spawn(&env, CmdOpts::default());
    assert_initialized(&second.initialize().await);
    assert_eq!(read_pid_file(&paths.pid_file).map(|p| p.pid), guard.pid());
    assert_eq!(run_dir_files(&paths.run_dir, "pid").len(), 1);
}

#[tokio::test]
async fn concurrent_proxies_spawn_one_daemon() {
    let env = TestEnv::new().await;
    let paths = env.paths().await;
    let mut proxies: Vec<_> = (0..5)
        .map(|_| ProxyClient::spawn(&env, CmdOpts::default()))
        .collect();
    let init = common::request_line(1, "initialize", &common::initialize_params());
    for proxy in &mut proxies {
        proxy.send(&init).await;
    }
    let guard = daemon_guard(&paths).await;
    for proxy in &mut proxies {
        assert_initialized(&proxy.response_within(1, Duration::from_secs(60)).await);
    }
    assert_eq!(
        run_dir_files(&paths.run_dir, "pid"),
        vec![paths.pid_file.clone()]
    );
    let pid = guard.pid().expect("daemon pid");
    assert!(ferrex_store::pid_alive(pid));
}

#[tokio::test]
async fn stale_socket_file_is_replaced() {
    let env = TestEnv::new().await;
    let paths = env.paths().await;
    std::fs::write(&paths.socket, "stale").unwrap();
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    assert_initialized(&proxy.initialize().await);
    let _guard = daemon_guard(&paths).await;
}

#[tokio::test]
async fn lock_held_by_killed_process_does_not_block() {
    let env = TestEnv::new().await;
    let paths = env.paths().await;
    let mut first = ProxyClient::spawn(
        &env,
        env_opts(&[("FERREX_TEST_DAEMON_BIND_DELAY_MS", "3000")]),
    );
    wait_until(
        "first proxy holds the spawn lock",
        Duration::from_secs(10),
        || paths.lock_file.exists() && !lock_is_free(&paths.lock_file),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    signal(first.pid(), rustix::process::Signal::KILL);
    first
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("killed proxy exits");

    let mut second = ProxyClient::spawn(&env, CmdOpts::default());
    assert_initialized(&second.initialize().await);
    let _guard = daemon_guard(&paths).await;
}

#[tokio::test]
async fn daemon_exit_before_bind_fails_fast() {
    let env = TestEnv::without_sidecar();
    let paths = env.paths().await;
    let mut proxy = ProxyClient::spawn(
        &env,
        env_opts(&[("FERREX_TEST_DAEMON_EXIT_BEFORE_BIND", "1")]),
    );
    let status = expect_exit(&mut proxy, Duration::from_secs(10)).await;
    assert!(!status.success(), "{status}");
    let stderr = proxy.stderr_text();
    assert!(stderr.contains("ferrexd.log"), "{stderr}");
    assert!(lock_is_free(&paths.lock_file));
}

/// Live daemons spawned for `key`. Uses `ps`: `pgrep -f` can't see arguments on recent macOS.
fn daemon_pids(key: &str) -> Vec<String> {
    let needle = format!("daemon --key {key}");
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,args="])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| line.contains(&needle))
        .map(str::to_owned)
        .collect()
}

/// Waits for the spawned daemon to die instead of sleeping past its bind delay.
async fn assert_child_never_binds(paths: &PathsJson) {
    wait_until("spawned daemon exits", Duration::from_secs(5), || {
        daemon_pids(&paths.key).is_empty()
    })
    .await;
    assert!(!paths.socket.exists(), "socket appeared");
    assert!(!paths.pid_file.exists(), "pid file appeared");
}

#[tokio::test]
async fn stdin_close_during_accept_poll_kills_child_and_releases_lock() {
    let env = TestEnv::without_sidecar();
    let paths = env.paths().await;
    let mut proxy = ProxyClient::spawn(
        &env,
        env_opts(&[("FERREX_TEST_DAEMON_BIND_DELAY_MS", "5000")]),
    );
    wait_until(
        "proxy holds the spawn lock",
        Duration::from_secs(10),
        || paths.lock_file.exists() && !lock_is_free(&paths.lock_file),
    )
    .await;
    wait_until("spawned daemon is running", Duration::from_secs(5), || {
        !daemon_pids(&paths.key).is_empty()
    })
    .await;
    proxy.close_stdin();
    let status = expect_exit(&mut proxy, Duration::from_secs(5)).await;
    assert!(status.success(), "{status}");
    assert!(lock_is_free(&paths.lock_file));
    assert_child_never_binds(&paths).await;
}

#[tokio::test]
async fn accept_timeout_kills_child_and_names_log() {
    let env = TestEnv::without_sidecar();
    let paths = env.paths().await;
    let mut proxy = ProxyClient::spawn(
        &env,
        env_opts(&[
            ("FERREX_TEST_DAEMON_BIND_DELAY_MS", "5000"),
            ("FERREX_TEST_PROXY_TIMEOUT_MS", "500"),
        ]),
    );
    let status = expect_exit(&mut proxy, Duration::from_secs(10)).await;
    assert!(!status.success(), "{status}");
    let stderr = proxy.stderr_text();
    assert!(stderr.contains("ferrexd.log"), "{stderr}");
    assert!(lock_is_free(&paths.lock_file));
    assert_child_never_binds(&paths).await;
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .map(|e| e.unwrap().path())
        .flat_map(|p| if p.is_dir() { files_under(&p) } else { vec![p] })
        .collect()
}

#[tokio::test]
async fn fresh_proxy_against_fake_leaves_no_state() {
    let env = TestEnv::without_sidecar();
    let models = env.root.join("models");
    let home = env.root.join("home");
    std::fs::create_dir(&models).unwrap();
    std::fs::create_dir(&home).unwrap();
    let mut opts = env_opts(&[
        ("FASTEMBED_CACHE_DIR", models.to_str().unwrap()),
        ("HOME", home.to_str().unwrap()),
    ]);
    opts.db_path = Some(env.root.join("fresh/ferrex.db"));
    let paths = env.paths_with(&opts).await;
    let fake = FakeDaemon::start(&paths.socket, vec![FakeConn::Serve(FakeBehaviour::Echo)]);
    let mut proxy = ProxyClient::spawn(&env, opts);
    let init = proxy
        .request("initialize", common::initialize_params())
        .await;
    assert_eq!(init["result"]["echo"], "initialize", "{init}");
    let list = proxy.request("tools/list", json!({})).await;
    assert_eq!(list["result"]["echo"], "tools/list", "{list}");
    assert_eq!(fake.connections(), 1);

    let files = files_under(&env.root);
    let state: Vec<_> = files
        .iter()
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            name.contains(".db") || name == "qdrant.pid" || p.starts_with(&models)
        })
        .collect();
    assert!(state.is_empty(), "{state:?}");
}

#[tokio::test]
async fn daemon_idles_out_after_last_proxy() {
    let env = TestEnv::new().await;
    let paths = env.paths().await;
    let mut proxy = ProxyClient::spawn(&env, env_opts(&[("FERREX_IDLE_TIMEOUT", "2")]));
    assert_initialized(&proxy.initialize().await);
    let mut guard = daemon_guard(&paths).await;
    let pid = guard.pid().expect("daemon pid");
    proxy.close_stdin();
    assert!(
        expect_exit(&mut proxy, Duration::from_secs(5))
            .await
            .success()
    );
    wait_until("daemon idles out", Duration::from_secs(15), || {
        !ferrex_store::pid_alive(pid)
    })
    .await;
    guard.disarm();
}
