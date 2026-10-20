mod common;

use std::process::ExitStatus;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use common::{
    CmdOpts, DaemonGuard, FakeBehaviour, FakeConn, FakeDaemon, PathsJson, ProxyClient, TestEnv,
    ferrex_bin, initialize_params, read_pid_file, request_line, signal,
};

const RESTART_CODE: i64 = -32603;
const RESTART_MESSAGE: &str = "ferrex daemon restarted, retry";
const LOSS_DIAG: &str = "daemon connection lost";
const LIST_CHANGED: &str = "notifications/tools/list_changed";
const WITHIN: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);

use FakeBehaviour::{CloseOnRequest, Echo, HoldFirst, NeverAnswer};
use FakeConn::{CloseBeforeReply, DelayHello, Refuse, Serve};

fn env_opts(vars: &[(&str, &str)]) -> CmdOpts {
    CmdOpts {
        env: vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        ..Default::default()
    }
}

async fn with_fake(script: Vec<FakeConn>) -> (TestEnv, FakeDaemon) {
    let env = TestEnv::without_sidecar();
    let paths = env.paths().await;
    let fake = FakeDaemon::start(&paths.socket, script);
    (env, fake)
}

/// An initialised proxy session against `fake`, which must answer `initialize`.
async fn initialised(env: &TestEnv, opts: CmdOpts) -> ProxyClient {
    let mut proxy = ProxyClient::spawn(env, opts);
    let init = proxy.initialize().await;
    assert_eq!(init["result"]["echo"], "initialize", "{init}");
    proxy
}

fn initialize_line() -> String {
    request_line(1, "initialize", &initialize_params())
}

fn initialized_line(params: &Value) -> String {
    json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": params}).to_string()
}

fn assert_echo(resp: &Value, method: &str) {
    assert_eq!(resp["result"]["echo"], method, "{resp}");
}

fn error_of(resp: &Value) -> (i64, String) {
    let code = resp["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected an error response: {resp}"));
    let message = resp["error"]["message"].as_str().unwrap_or_default();
    (code, message.to_owned())
}

fn assert_restart_error(resp: &Value) {
    assert_eq!(
        error_of(resp),
        (RESTART_CODE, RESTART_MESSAGE.to_owned()),
        "{resp}"
    );
}

/// Sends a `tools/call` the fake closes the connection on, and checks its restart error.
async fn lose_daemon(proxy: &mut ProxyClient) {
    let resp = proxy.request("tools/call", json!({"name": "lose"})).await;
    assert_restart_error(&resp);
}

async fn expect_exit(proxy: &mut ProxyClient, within: Duration) -> ExitStatus {
    proxy.wait_exit(within).await.unwrap_or_else(|| {
        panic!(
            "proxy did not exit within {within:?}; stderr:\n{}",
            proxy.stderr_text()
        )
    })
}

fn count_method(messages: &[Value], method: &str) -> usize {
    messages.iter().filter(|m| m["method"] == method).count()
}

fn count_lines(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|l| l.contains(needle)).count()
}

/// `tools/call` params carrying complete `_meta`, so rmcp picks the inline lifecycle.
fn inline_params() -> Value {
    json!({
        "name": "recall",
        "_meta": {
            "protocolVersion": "2026-07-28",
            "clientInfo": {"name": "test", "version": "0.1.0"},
            "clientCapabilities": {}
        }
    })
}

/// Waits until connection `idx` of `fake` has received at least `n` lines.
async fn wait_received(fake: &FakeDaemon, idx: usize, n: usize) {
    wait_until(
        &format!("connection {idx} receives {n} lines"),
        WITHIN,
        || {
            fake.received()
                .get(idx)
                .is_some_and(|lines| lines.len() >= n)
        },
    )
    .await;
}

async fn wait_until(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {within:?}");
        tokio::time::sleep(POLL).await;
    }
}

async fn change_key(env: &TestEnv, before: &PathsJson) -> PathsJson {
    let toml = std::fs::read_to_string(&env.config_path).unwrap();
    let edited = toml.replacen("threshold = 0.95", "threshold = 0.9", 1);
    assert_ne!(toml, edited, "dedup threshold not found in ferrex.toml");
    std::fs::write(&env.config_path, edited).unwrap();
    let after = env.paths().await;
    assert_ne!(after.key, before.key);
    after
}

#[tokio::test]
async fn initialized_during_reconnect_sent_once() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(2)), DelayHello(500, Echo)]).await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    let id = proxy.send_request("tools/list", &json!({})).await;
    proxy.notify("notifications/initialized", json!({})).await;
    assert_echo(&proxy.response_within(id, WITHIN).await, "tools/list");
    let second = &fake.received()[1];
    assert_eq!(
        count_lines(second, "notifications/initialized"),
        1,
        "{second:?}"
    );
}

#[tokio::test]
async fn in_flight_request_gets_restart_error() {
    let (env, _fake) = with_fake(vec![Serve(CloseOnRequest(2)), Serve(Echo)]).await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    assert!(proxy.is_running());
}

#[tokio::test]
async fn next_request_reconnects_and_replays() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(2)), Serve(Echo)]).await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    let resp = proxy.request("tools/list", json!({})).await;
    assert_echo(&resp, "tools/list");
    assert_eq!(
        fake.received()[1],
        vec![
            initialize_line(),
            initialized_line(&json!({})),
            request_line(3, "tools/list", &json!({})),
        ]
    );
    let init_responses = proxy
        .seen()
        .iter()
        .filter(|m| m["result"]["echo"] == "initialize")
        .count();
    assert_eq!(init_responses, 1, "{:?}", proxy.seen());
}

#[tokio::test]
async fn lines_during_reconnect_arrive_in_order() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(2)), DelayHello(500, Echo)]).await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    let methods = ["tools/list", "ping", "resources/list"];
    let mut ids = Vec::new();
    for method in methods {
        ids.push(proxy.send_request(method, &json!({})).await);
    }
    for (id, method) in ids.iter().zip(methods) {
        assert_echo(&proxy.response_within(*id, WITHIN).await, method);
    }
    let expected: Vec<String> = [initialize_line(), initialized_line(&json!({}))]
        .into_iter()
        .chain(
            ids.iter()
                .zip(methods)
                .map(|(id, m)| request_line(*id, m, &json!({}))),
        )
        .collect();
    assert_eq!(fake.received()[1], expected);
}

#[tokio::test]
async fn key_change_sends_list_changed() {
    let env = TestEnv::without_sidecar();
    let first = env.paths().await;
    let _old = FakeDaemon::start(&first.socket, vec![Serve(CloseOnRequest(2))]);
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    let second = change_key(&env, &first).await;
    let new = FakeDaemon::start(&second.socket, vec![Serve(Echo)]);
    assert_echo(&proxy.request("tools/list", json!({})).await, "tools/list");
    assert_eq!(count_method(proxy.seen(), LIST_CHANGED), 1);
    assert_eq!(new.connections(), 1);
}

#[tokio::test]
async fn same_key_reconnect_sends_no_list_changed() {
    let (env, _fake) = with_fake(vec![Serve(CloseOnRequest(2)), Serve(Echo)]).await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    assert_echo(&proxy.request("tools/list", json!({})).await, "tools/list");
    assert_eq!(count_method(proxy.seen(), LIST_CHANGED), 0);
}

#[tokio::test]
async fn key_change_then_same_key_reconnect_sends_list_changed_once() {
    let env = TestEnv::without_sidecar();
    let first = env.paths().await;
    let _old = FakeDaemon::start(&first.socket, vec![Serve(CloseOnRequest(2))]);
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    let second = change_key(&env, &first).await;
    // Replayed initialize, then the tools/list, then the call it closes on.
    let new = FakeDaemon::start(&second.socket, vec![Serve(CloseOnRequest(3)), Serve(Echo)]);
    assert_echo(&proxy.request("tools/list", json!({})).await, "tools/list");
    lose_daemon(&mut proxy).await;
    assert_echo(&proxy.request("tools/list", json!({})).await, "tools/list");
    assert_eq!(new.connections(), 2);
    assert_eq!(count_method(proxy.seen(), LIST_CHANGED), 1);
}

#[tokio::test]
async fn protocol_refusal_after_init_returns_mcp_hint() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(2)), Refuse("protocol")]).await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    let resp = proxy.request("tools/list", json!({})).await;
    let (code, message) = error_of(&resp);
    assert_eq!(code, RESTART_CODE);
    assert!(message.contains("/mcp reconnect"), "{message}");
    assert_eq!(fake.connections(), 2);
    assert!(proxy.is_running());
}

#[tokio::test]
async fn failed_round_then_next_request_succeeds() {
    let (env, fake) = with_fake(vec![
        Serve(CloseOnRequest(2)),
        CloseBeforeReply,
        CloseBeforeReply,
        CloseBeforeReply,
        Serve(Echo),
    ])
    .await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    let failed = proxy.request("tools/list", json!({})).await;
    let (code, message) = error_of(&failed);
    assert_eq!(code, RESTART_CODE);
    assert!(message.contains("closed the connection"), "{message}");
    assert!(!message.contains("/mcp reconnect"), "{message}");
    assert_eq!(fake.connections(), 4);
    assert_echo(&proxy.request("tools/list", json!({})).await, "tools/list");
    assert_eq!(fake.connections(), 5);
}

#[tokio::test]
async fn requests_during_failed_round_each_get_one_error() {
    let (env, fake) = with_fake(vec![
        Serve(CloseOnRequest(2)),
        CloseBeforeReply,
        CloseBeforeReply,
        CloseBeforeReply,
        Serve(Echo),
    ])
    .await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(
            proxy
                .send_request("tools/list", &json!({"queued": true}))
                .await,
        );
    }
    for resp in proxy.responses_within(&ids, WITHIN).await {
        assert_eq!(error_of(&resp).0, RESTART_CODE, "{resp}");
    }
    assert_echo(&proxy.request("ping", json!({})).await, "ping");
    for id in &ids {
        let answers = proxy.seen().iter().filter(|m| m["id"] == json!(id)).count();
        assert_eq!(answers, 1, "id {id}: {:?}", proxy.seen());
    }
    let last = fake.received().pop().unwrap();
    assert_eq!(count_lines(&last, "queued"), 0, "{last:?}");
    assert_eq!(last.len(), 3, "{last:?}");
}

#[tokio::test]
async fn stalled_replay_ends_round() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(2)), Serve(NeverAnswer)]).await;
    let opts = env_opts(&[("FERREX_TEST_PROXY_TIMEOUT_MS", "500")]);
    let mut proxy = initialised(&env, opts).await;
    lose_daemon(&mut proxy).await;
    let resp = proxy.request("tools/list", json!({})).await;
    assert_eq!(error_of(&resp).0, RESTART_CODE, "{resp}");
    assert_eq!(fake.connections(), 2);
    assert!(proxy.is_running());
}

#[tokio::test]
async fn eof_during_replay_is_retried() {
    let (env, fake) = with_fake(vec![
        Serve(CloseOnRequest(2)),
        Serve(CloseOnRequest(1)),
        Serve(Echo),
    ])
    .await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    assert_echo(&proxy.request("tools/list", json!({})).await, "tools/list");
    assert_eq!(fake.connections(), 3);
}

#[tokio::test]
async fn notifications_while_disconnected_are_dropped() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(2)), Serve(Echo)]).await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    proxy
        .notify("notifications/cancelled", json!({"requestId": 2}))
        .await;
    let marker = json!({"marker": "while-disconnected"});
    proxy
        .notify("notifications/initialized", marker.clone())
        .await;
    assert_echo(&proxy.request("tools/list", json!({})).await, "tools/list");
    let second = &fake.received()[1];
    assert_eq!(
        count_lines(second, "notifications/cancelled"),
        0,
        "{second:?}"
    );
    assert!(second.contains(&initialized_line(&marker)), "{second:?}");
}

#[tokio::test]
async fn stdin_eof_while_disconnected_exits_zero() {
    let (env, _fake) = with_fake(vec![Serve(CloseOnRequest(2)), Serve(Echo)]).await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    proxy.close_stdin();
    let status = expect_exit(&mut proxy, WITHIN).await;
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn inline_lifecycle_reconnects_without_replay() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(2)), Serve(Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let meta = inline_params();
    assert_echo(
        &proxy.request("tools/call", meta.clone()).await,
        "tools/call",
    );
    lose_daemon(&mut proxy).await;
    let id = proxy.send_request("tools/call", &meta).await;
    assert_echo(&proxy.response_within(id, WITHIN).await, "tools/call");
    let second = &fake.received()[1];
    assert_eq!(second.first(), Some(&request_line(id, "tools/call", &meta)));
    assert_eq!(count_lines(second, "\"initialize\""), 0, "{second:?}");
}

#[tokio::test]
async fn stdin_eof_during_reconnect_round_exits_zero() {
    let (env, _fake) = with_fake(vec![Serve(CloseOnRequest(2)), DelayHello(2000, Echo)]).await;
    let mut proxy = initialised(&env, CmdOpts::default()).await;
    lose_daemon(&mut proxy).await;
    proxy.send_request("tools/list", &json!({})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let closed = Instant::now();
    proxy.close_stdin();
    let status = expect_exit(&mut proxy, WITHIN).await;
    assert!(status.success(), "{status}");
    assert!(
        closed.elapsed() < Duration::from_millis(1500),
        "{:?}",
        closed.elapsed()
    );
}

#[tokio::test]
async fn missing_exe_after_init_returns_mcp_hint() {
    let env = TestEnv::without_sidecar();
    let copy = env.root.join("ferrex-copy");
    std::fs::copy(ferrex_bin(), &copy).unwrap();
    let opts = CmdOpts {
        exe: Some(copy.clone()),
        ..Default::default()
    };
    let paths = env.paths_with(&opts).await;
    let fake = FakeDaemon::start(&paths.socket, vec![Serve(CloseOnRequest(2))]);
    let mut proxy = initialised(&env, opts).await;
    std::fs::remove_file(&copy).unwrap();
    lose_daemon(&mut proxy).await;
    let resp = proxy.request("tools/list", json!({})).await;
    let (code, message) = error_of(&resp);
    assert_eq!(code, RESTART_CODE);
    assert!(message.contains("/mcp reconnect"), "{message}");
    assert!(proxy.is_running());
    assert_eq!(fake.connections(), 1);
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

fn sleeper_with_pid_file(env: &TestEnv) -> (tokio::process::Child, std::path::PathBuf) {
    let child = tokio::process::Command::new("sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .expect("spawn sleep");
    let pid_file = env.root.join("sleeper.pid");
    let pid = child.id().expect("sleeper pid");
    std::fs::write(&pid_file, json!({"pid": pid, "db_path": "/x"}).to_string()).unwrap();
    (child, pid_file)
}

#[tokio::test]
async fn lazy_guard_kills_the_pid_file_daemon() {
    let env = TestEnv::without_sidecar();
    let (mut child, pid_file) = sleeper_with_pid_file(&env);
    drop(DaemonGuard::lazy(&pid_file, 1));
    let status = tokio::time::timeout(WITHIN, child.wait())
        .await
        .expect("sleeper killed")
        .unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9)
    );
}

#[tokio::test]
async fn lazy_guard_spares_the_excluded_pid() {
    let env = TestEnv::without_sidecar();
    let (mut child, pid_file) = sleeper_with_pid_file(&env);
    drop(DaemonGuard::lazy(
        &pid_file,
        child.id().expect("sleeper pid"),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "excluded pid was killed"
    );
}

#[tokio::test]
async fn real_daemon_kill9_next_request_succeeds() {
    let env = TestEnv::new().await;
    let paths = env.paths().await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let init = proxy.initialize().await;
    assert!(init["result"]["serverInfo"].is_object(), "{init}");
    let mut first = DaemonGuard::from_pid_file(&paths.pid_file, Duration::from_secs(30)).await;
    let first_pid = first.pid().expect("daemon pid");
    let _respawned = DaemonGuard::lazy(&paths.pid_file, first_pid);
    let stored = call_tool(
        &mut proxy,
        "store",
        json!({"content": "Rotated the staging TLS certificates", "memory_type": "episodic"}),
    )
    .await;
    assert_eq!(stored["stored"], true, "{stored}");

    signal(first_pid, rustix::process::Signal::KILL);
    first.disarm();
    wait_until("proxy notices the daemon loss", WITHIN, || {
        proxy.stderr_text().contains(LOSS_DIAG)
    })
    .await;

    let recalled = call_tool(&mut proxy, "recall", json!({"query": "TLS certificates"})).await;
    wait_until("the new daemon writes its PID file", WITHIN, || {
        read_pid_file(&paths.pid_file).is_some_and(|p| p.pid != first_pid)
    })
    .await;
    let hits = recalled.as_array().expect("recall returns an array");
    assert!(hits.iter().any(|r| r["id"] == stored["id"]), "{recalled}");
    let init_responses = proxy
        .seen()
        .iter()
        .filter(|m| m["result"]["serverInfo"].is_object())
        .count();
    assert_eq!(init_responses, 1, "{:?}", proxy.seen());
}

#[tokio::test]
async fn pre_init_key_change_sends_no_list_changed() {
    let env = TestEnv::without_sidecar();
    let first = env.paths().await;
    let old = FakeDaemon::start(&first.socket, vec![Serve(NeverAnswer)]);
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let id = proxy.send_request("initialize", &initialize_params()).await;
    wait_received(&old, 0, 1).await;
    let second = change_key(&env, &first).await;
    let new = FakeDaemon::start(&second.socket, vec![Serve(Echo)]);
    drop(old);
    assert_echo(&proxy.response_within(id, WITHIN).await, "initialize");
    assert_eq!(proxy.seen().len(), 1, "{:?}", proxy.seen());
    assert_echo(&proxy.request("tools/list", json!({})).await, "tools/list");
    assert_eq!(
        count_method(proxy.seen(), LIST_CHANGED),
        0,
        "{:?}",
        proxy.seen()
    );
    assert_eq!(new.received()[0][0], initialize_line());
}

#[tokio::test]
async fn ping_in_flight_before_init_gets_restart_error() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(1)), Serve(Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    assert_restart_error(&proxy.request("ping", json!({})).await);
    let init = proxy.request("initialize", initialize_params()).await;
    assert_echo(&init, "initialize");
    let second = &fake.received()[1];
    assert_eq!(count_lines(second, "\"initialize\""), 1, "{second:?}");
}

#[tokio::test]
async fn initialized_during_pre_init_reconnect_reaches_daemon() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(1)), DelayHello(500, Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    assert_restart_error(&proxy.request("ping", json!({})).await);
    let id = proxy.send_request("initialize", &initialize_params()).await;
    proxy.notify("notifications/initialized", json!({})).await;
    assert_echo(&proxy.response_within(id, WITHIN).await, "initialize");
    wait_received(&fake, 1, 2).await;
    assert_eq!(
        fake.received()[1],
        vec![
            request_line(id, "initialize", &initialize_params()),
            initialized_line(&json!({})),
        ]
    );
}

#[tokio::test]
async fn stalled_forwarded_initialize_ends_round_after_one_attempt() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(1)), Serve(NeverAnswer)]).await;
    let opts = env_opts(&[("FERREX_TEST_PROXY_TIMEOUT_MS", "500")]);
    let mut proxy = ProxyClient::spawn(&env, opts);
    proxy.send_request("initialize", &initialize_params()).await;
    let status = expect_exit(&mut proxy, WITHIN).await;
    assert!(!status.success(), "{status}");
    assert_eq!(fake.connections(), 2);
}

#[tokio::test]
async fn close_before_initialize_response_is_retried() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(1)), Serve(Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let id = proxy.send_request("initialize", &initialize_params()).await;
    assert_echo(&proxy.response_within(id, WITHIN).await, "initialize");
    assert_echo(&proxy.request("tools/list", json!({})).await, "tools/list");
    let answers = proxy.seen().iter().filter(|m| m["id"] == json!(id)).count();
    assert_eq!(answers, 1, "{:?}", proxy.seen());
    assert_eq!(fake.received()[1][0], initialize_line());
    assert_eq!(count_lines(&fake.received()[1], "\"initialize\""), 1);
}

#[tokio::test]
async fn pre_init_failed_round_exits_nonzero() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(1))]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    proxy.send_request("initialize", &initialize_params()).await;
    let status = expect_exit(&mut proxy, WITHIN).await;
    assert!(!status.success(), "{status}");
    assert_eq!(fake.connections(), 4);
    for lines in fake.received() {
        assert_eq!(lines, vec![initialize_line()]);
    }
}

#[tokio::test]
async fn stdin_eof_while_reconnect_pending_exits_zero() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(1)), DelayHello(2000, Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    proxy.send_request("initialize", &initialize_params()).await;
    wait_until("the proxy reconnects at once", WITHIN, || {
        fake.connections() == 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let closed = Instant::now();
    proxy.close_stdin();
    let status = expect_exit(&mut proxy, WITHIN).await;
    assert!(status.success(), "{status}");
    assert!(
        closed.elapsed() < Duration::from_millis(1500),
        "{:?}",
        closed.elapsed()
    );
    assert_eq!(proxy.read_line_timeout(Duration::from_secs(1)).await, None);
}

#[tokio::test]
async fn inline_first_request_resent_after_close() {
    let (env, fake) = with_fake(vec![Serve(CloseOnRequest(1)), Serve(Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let id = proxy.send_request("tools/call", &inline_params()).await;
    assert_echo(&proxy.response_within(id, WITHIN).await, "tools/call");
    let kept = request_line(id, "tools/call", &inline_params());
    assert_eq!(fake.received(), vec![vec![kept.clone()], vec![kept]]);
}

#[tokio::test]
async fn inline_pending_resend_lets_later_lines_through() {
    let (env, fake) = with_fake(vec![
        Serve(CloseOnRequest(1)),
        Serve(HoldFirst { close_on: None }),
    ])
    .await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let kept = proxy.send_request("tools/call", &inline_params()).await;
    wait_received(&fake, 1, 1).await;
    let list = proxy.send_request("tools/list", &json!({})).await;
    assert_echo(&proxy.response_within(list, WITHIN).await, "tools/list");
    let cancelled = json!({"requestId": kept});
    proxy
        .notify("notifications/cancelled", cancelled.clone())
        .await;
    wait_received(&fake, 1, 3).await;
    assert_eq!(
        fake.received()[1],
        vec![
            request_line(kept, "tools/call", &inline_params()),
            request_line(list, "tools/list", &json!({})),
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": cancelled})
                .to_string(),
        ]
    );
    let kept_answers = proxy
        .seen()
        .iter()
        .filter(|m| m["id"] == json!(kept))
        .count();
    assert_eq!(kept_answers, 0, "{:?}", proxy.seen());
}

#[tokio::test]
async fn inline_repeated_loss_resends_alone_then_exits() {
    let (env, fake) = with_fake(vec![
        Serve(CloseOnRequest(1)),
        Serve(HoldFirst { close_on: Some(2) }),
    ])
    .await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let kept = proxy.send_request("tools/call", &inline_params()).await;
    let kept_line = request_line(kept, "tools/call", &inline_params());
    for conn in 1..=3 {
        wait_received(&fake, conn, 1).await;
        assert_eq!(fake.received()[conn], vec![kept_line.clone()]);
        let id = proxy.send_request("tools/list", &json!({})).await;
        assert_restart_error(&proxy.response_within(id, WITHIN).await);
    }
    let status = expect_exit(&mut proxy, WITHIN).await;
    assert!(!status.success(), "{status}");
    assert_eq!(fake.connections(), 4);
    assert!(
        proxy
            .stderr_text()
            .contains("ferrex daemon connection lost")
    );
}

#[tokio::test]
async fn inline_later_success_initialises() {
    let (env, fake) = with_fake(vec![
        Serve(CloseOnRequest(1)),
        Serve(HoldFirst { close_on: Some(3) }),
        Serve(Echo),
    ])
    .await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let kept = proxy.send_request("tools/call", &inline_params()).await;
    wait_received(&fake, 1, 1).await;
    assert_echo(&proxy.request("tools/list", json!({})).await, "tools/list");
    let lost = proxy.send_request("tools/list", &json!({})).await;
    for resp in proxy.responses_within(&[kept, lost], WITHIN).await {
        assert_restart_error(&resp);
    }
    assert_echo(&proxy.request("ping", json!({})).await, "ping");
    assert!(proxy.is_running());
    assert_eq!(fake.connections(), 3);
    assert_eq!(count_lines(&fake.received()[2], "tools/call"), 0);
}
