mod common;

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use common::{
    CmdOpts, FakeBehaviour, FakeConn, FakeDaemon, ProxyClient, TestEnv, initialize_params,
    request_line,
};
use pretty_assertions::assert_eq;

const EXIT_WITHIN: Duration = Duration::from_secs(15);

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

async fn expect_exit(proxy: &mut ProxyClient, within: Duration) -> ExitStatus {
    proxy.wait_exit(within).await.unwrap_or_else(|| {
        panic!(
            "proxy did not exit within {within:?}; stderr:\n{}",
            proxy.stderr_text()
        )
    })
}

fn echoed(resp: &Value, method: &str) -> Value {
    assert_eq!(resp["result"]["echo"], method, "{resp}");
    resp["result"]["params"].clone()
}

#[tokio::test]
async fn fake_echo_round_trip() {
    let (env, fake) = with_fake(vec![FakeConn::Serve(FakeBehaviour::Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let init = proxy.request("initialize", initialize_params()).await;
    assert_eq!(echoed(&init, "initialize"), initialize_params());
    let list = proxy.request("tools/list", json!({"cursor": "x"})).await;
    assert_eq!(echoed(&list, "tools/list"), json!({"cursor": "x"}));
    assert_eq!(fake.connections(), 1);
}

#[tokio::test]
async fn non_utf8_line_does_not_stop_proxy() {
    let (env, fake) = with_fake(vec![FakeConn::Serve(FakeBehaviour::Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    proxy.send(b"\xff\n").await;
    let resp = proxy.request("initialize", initialize_params()).await;
    echoed(&resp, "initialize");
    assert_eq!(fake.received()[0][0], "\u{fffd}");
}

#[tokio::test]
async fn lines_before_ok_are_queued_in_order() {
    let (env, fake) = with_fake(vec![FakeConn::DelayHello(500, FakeBehaviour::Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let lines = [
        request_line(1, "initialize", &initialize_params()),
        request_line(2, "tools/list", &json!({})),
        request_line(3, "ping", &json!({})),
    ];
    for line in &lines {
        proxy.send(line).await;
    }
    for id in 1..=3 {
        proxy.response_within(id, Duration::from_secs(10)).await;
    }
    assert_eq!(fake.received(), vec![lines.to_vec()]);
}

#[tokio::test]
async fn large_line_round_trips() {
    let (env, fake) = with_fake(vec![FakeConn::Serve(FakeBehaviour::Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let big = "x".repeat(5 * 1024 * 1024);
    let params = json!({"content": big});
    let resp = proxy
        .request_within("tools/call", params.clone(), Duration::from_secs(10))
        .await;
    assert!(
        echoed(&resp, "tools/call") == params,
        "echoed params differ"
    );
    let received = fake.received();
    assert!(
        received[0][0] == request_line(1, "tools/call", &params),
        "received line differs from the sent one"
    );
}

async fn refusal_connections(kind: &'static str) -> usize {
    let (env, fake) = with_fake(vec![FakeConn::Refuse(kind)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let status = expect_exit(&mut proxy, EXIT_WITHIN).await;
    assert!(!status.success(), "{status}");
    assert!(
        proxy.stderr_text().contains("fake daemon refused"),
        "{}",
        proxy.stderr_text()
    );
    fake.connections()
}

#[tokio::test]
async fn three_key_refusals_exit_nonzero() {
    assert_eq!(refusal_connections("key").await, 3);
}

#[tokio::test]
async fn protocol_refusal_exits_after_one_attempt() {
    assert_eq!(refusal_connections("protocol").await, 1);
}

#[tokio::test]
async fn init_refusal_exits_after_one_attempt() {
    assert_eq!(refusal_connections("init").await, 1);
}

#[tokio::test]
async fn eof_before_hello_reply_is_retried() {
    let (env, fake) = with_fake(vec![
        FakeConn::CloseBeforeReply,
        FakeConn::Serve(FakeBehaviour::Echo),
    ])
    .await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let resp = proxy.request("initialize", initialize_params()).await;
    echoed(&resp, "initialize");
    assert_eq!(fake.connections(), 2);
    assert_eq!(fake.received()[0], Vec::<String>::new());
}

#[tokio::test]
async fn silent_hello_ends_round_after_one_attempt() {
    let (env, fake) = with_fake(vec![FakeConn::NeverReply]).await;
    let mut proxy = ProxyClient::spawn(&env, env_opts(&[("FERREX_TEST_PROXY_TIMEOUT_MS", "500")]));
    let status = expect_exit(&mut proxy, EXIT_WITHIN).await;
    assert!(!status.success(), "{status}");
    assert_eq!(fake.connections(), 1);
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[tokio::test]
async fn flock_timeout_ends_round() {
    let env = TestEnv::without_sidecar();
    let paths = env.paths().await;
    let held = std::fs::File::create(&paths.lock_file).unwrap();
    held.lock().unwrap();
    let mut proxy = ProxyClient::spawn(&env, env_opts(&[("FERREX_TEST_PROXY_TIMEOUT_MS", "500")]));
    let status = expect_exit(&mut proxy, Duration::from_secs(3)).await;
    assert!(!status.success(), "{status}");
    let stderr = proxy.stderr_text();
    let lock = paths.lock_file.to_str().unwrap();
    assert_eq!(occurrences(&stderr, lock), 1, "{stderr}");
}

#[tokio::test]
async fn unusable_run_dir_fails_fast() {
    let env = TestEnv::without_sidecar();
    let file = env.root.join("not-a-dir");
    std::fs::write(&file, "").unwrap();
    let file = file.to_str().unwrap();
    let mut proxy = ProxyClient::spawn(&env, env_opts(&[("FERREX_RUN_DIR", file)]));
    let status = expect_exit(&mut proxy, Duration::from_secs(10)).await;
    assert!(!status.success(), "{status}");
    let stderr = proxy.stderr_text();
    assert_eq!(occurrences(&stderr, file), 1, "{stderr}");
}

#[tokio::test]
async fn no_daemon_env_zero_uses_daemon() {
    let (env, fake) = with_fake(vec![FakeConn::Serve(FakeBehaviour::Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, env_opts(&[("FERREX_NO_DAEMON", "0")]));
    let resp = proxy.request("initialize", initialize_params()).await;
    echoed(&resp, "initialize");
    assert_eq!(fake.connections(), 1);
}

#[tokio::test]
async fn memory_db_path_selects_standalone() {
    let env = TestEnv::without_sidecar();
    let opts = CmdOpts {
        db_path: Some(":memory:".into()),
        ..Default::default()
    };
    let paths = env.paths_with(&opts).await;
    let fake = FakeDaemon::start(&paths.socket, vec![FakeConn::Serve(FakeBehaviour::Echo)]);
    let mut proxy = ProxyClient::spawn(&env, opts);
    let status = expect_exit(&mut proxy, Duration::from_secs(60)).await;
    assert!(!status.success(), "{status}");
    let stderr = proxy.stderr_text();
    assert!(!stderr.contains("ferrex ready"), "{stderr}");
    assert_eq!(fake.connections(), 0);
    let leftovers: Vec<_> = std::fs::read_dir(&paths.run_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext == "sock" || ext == "pid")
        })
        .filter(|p| *p != paths.socket)
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(!paths.pid_file.exists());
}

#[tokio::test]
async fn closed_stderr_does_not_panic() {
    let (env, fake) = with_fake(vec![FakeConn::Refuse("protocol")]).await;
    let mut cmd = env.ferrex_cmd();
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stderr.take());
    // tokio's `wait` closes a stdin it still owns, which the proxy would see as EOF.
    let _stdin = child.stdin.take();
    let status = tokio::time::timeout(EXIT_WITHIN, child.wait())
        .await
        .expect("proxy exits")
        .unwrap();
    assert_eq!(status.code(), Some(1), "{status}");
    assert_eq!(fake.connections(), 1);
}

#[tokio::test]
async fn exit_with_blocked_stdout_does_not_hang() {
    let (env, fake) = with_fake(vec![FakeConn::Serve(FakeBehaviour::Echo)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    let params = json!({"content": "x".repeat(1024 * 1024)});
    for id in 1..=4 {
        proxy.send(request_line(id, "tools/call", &params)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while fake.received().first().map_or(0, Vec::len) < 4 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "fake never got all requests"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Lets the echoes reach the proxy, whose writer then blocks on the full, unread stdout pipe.
    tokio::time::sleep(Duration::from_millis(500)).await;
    proxy.close_stdin();
    expect_exit(&mut proxy, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn stdin_eof_waits_for_in_flight_response() {
    let (env, _fake) = with_fake(vec![FakeConn::Serve(FakeBehaviour::DelayMs(500))]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    proxy.initialize().await;
    let id = proxy
        .send_request("tools/call", &json!({"name": "store"}))
        .await;
    proxy.close_stdin();
    let resp = proxy.response_within(id, Duration::from_secs(5)).await;
    echoed(&resp, "tools/call");
    let status = expect_exit(&mut proxy, EXIT_WITHIN).await;
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn stdin_eof_drain_is_bounded() {
    let (env, fake) = with_fake(vec![FakeConn::Serve(FakeBehaviour::HoldFirst {
        close_on: None,
    })])
    .await;
    let mut proxy = ProxyClient::spawn(&env, env_opts(&[("FERREX_TEST_PROXY_TIMEOUT_MS", "500")]));
    proxy.send_request("tools/call", &json!({})).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while fake.received().first().map_or(0, Vec::len) < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "fake never got the request"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    proxy.close_stdin();
    let status = expect_exit(&mut proxy, Duration::from_secs(5)).await;
    assert!(status.success(), "{status}");
}

/// Polls `sample` until it holds still for a second; returns the settled value.
async fn settled(within: Duration, sample: impl Fn() -> usize) -> usize {
    let deadline = tokio::time::Instant::now() + within;
    let mut last = sample();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = sample();
        if now == last && now > 0 {
            return now;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never settled, last {now}"
        );
        last = now;
    }
}

#[tokio::test]
async fn unread_stdout_back_pressures_the_daemon() {
    const LINES: usize = 64;
    let flood = FakeBehaviour::Flood {
        lines: LINES,
        bytes: 1024 * 1024,
    };
    let (env, fake) = with_fake(vec![FakeConn::Serve(flood)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    proxy.send_request("tools/call", &json!({})).await;
    let written = settled(Duration::from_secs(20), || fake.flooded()).await;
    // The proxy's few-MB queue plus the socket and pipe buffers hold well under 16 such lines.
    assert!(
        written < 16,
        "proxy kept draining the socket: {written} of {LINES} lines written"
    );
    proxy.close_stdin();
    expect_exit(&mut proxy, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn response_held_for_budget_survives_daemon_loss() {
    const LINES: usize = 3;
    const MIB: usize = 1024 * 1024;
    let behaviour = FakeBehaviour::FloodAnswerClose {
        request: 2,
        lines: LINES,
        bytes: MIB,
    };
    let (env, fake) = with_fake(vec![FakeConn::Serve(behaviour)]).await;
    let mut proxy = ProxyClient::spawn(&env, CmdOpts::default());
    proxy.initialize().await;
    // Three 1 MiB notifications fill most of the 4 MiB budget, so the 1 MiB answer waits for it.
    let params = json!({"content": "x".repeat(MIB)});
    let call = proxy.send_request("tools/call", &params).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while fake.flooded() < LINES + 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "fake never sent the answer"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Writing to the closed socket is how the proxy learns of the loss.
    let ping = proxy.send_request("ping", &json!({})).await;
    let responses = proxy
        .responses_within(&[call, ping], Duration::from_secs(20))
        .await;
    assert!(
        responses[0]["result"]["params"] == params,
        "the held answer was replaced: {}",
        responses[0]["error"]
    );
    assert_eq!(responses[1]["error"]["code"], -32603, "{}", responses[1]);
}
