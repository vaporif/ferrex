#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrex_store::{QdrantSidecar, SidecarPolicy};
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(50);

pub struct TestEnv {
    pub temp: TempDir,
    pub root: PathBuf,
    pub run_dir: PathBuf,
    pub db_path: PathBuf,
    pub config_path: PathBuf,
    pub qdrant_port: u16,
    _sidecar: Option<QdrantSidecar>,
}

#[derive(Debug, Clone, Default)]
pub struct CmdOpts {
    pub db_path: Option<PathBuf>,
    /// `Some(None)` drops `--qdrant-url`, so the binary starts its own sidecar.
    #[allow(
        clippy::option_option,
        reason = "unset, explicitly absent and set all differ"
    )]
    pub qdrant_url: Option<Option<String>>,
    /// Appended after every top-level flag; starts with the subcommand.
    pub extra_args: Vec<OsString>,
    pub env: Vec<(String, String)>,
    /// Runs this binary instead of the cargo-built `ferrex`.
    pub exe: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PathsJson {
    pub key: String,
    pub socket: PathBuf,
    pub pid_file: PathBuf,
    pub lock_file: PathBuf,
    pub run_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PidFileJson {
    pub pid: u32,
    pub db_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelloReplyJson {
    pub ok: bool,
    pub kind: Option<String>,
    pub error: Option<String>,
}

impl TestEnv {
    pub async fn new() -> Self {
        let env = Self::without_sidecar();
        let sidecar = QdrantSidecar::start(
            "qdrant",
            env.qdrant_port,
            Some(env.root.join("qdrant")),
            SidecarPolicy::KillOnDrop,
        )
        .await
        .expect("start test qdrant");
        Self {
            _sidecar: Some(sidecar),
            ..env
        }
    }

    pub fn without_sidecar() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().canonicalize().expect("canonical temp dir");
        Self {
            run_dir: root.join("run"),
            db_path: root.join("ferrex.db"),
            config_path: root.join("ferrex.toml"),
            qdrant_port: portpicker::pick_unused_port().expect("free port"),
            root,
            temp,
            _sidecar: None,
        }
    }

    pub fn qdrant_url(&self) -> String {
        format!("http://localhost:{}", self.qdrant_port)
    }

    pub fn ferrex_cmd(&self) -> Command {
        self.ferrex_cmd_with(CmdOpts::default())
    }

    pub fn ferrex_cmd_with(&self, opts: CmdOpts) -> Command {
        let mut cmd = Command::new(opts.exe.unwrap_or_else(|| ferrex_bin().into()));
        for (name, _) in std::env::vars_os() {
            let is_ferrex = name.to_str().is_some_and(|n| n.starts_with("FERREX_"));
            if is_ferrex && name != "FERREX_LOG" {
                cmd.env_remove(&name);
            }
        }
        if let Some(url) = opts.qdrant_url.unwrap_or_else(|| Some(self.qdrant_url())) {
            cmd.arg("--qdrant-url").arg(url);
        }
        cmd.arg("--db-path")
            .arg(opts.db_path.as_deref().unwrap_or(&self.db_path))
            .arg("--config-path")
            .arg(&self.config_path)
            .args(["--model-tier", "small"])
            .args(&opts.extra_args)
            .env("FERREX_RUN_DIR", &self.run_dir)
            .env("FERREX_IDLE_TIMEOUT", "3")
            .envs(opts.env)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        cmd
    }

    pub async fn paths(&self) -> PathsJson {
        self.paths_with(&CmdOpts::default()).await
    }

    /// Runs `daemon-paths` with `opts`' overrides, so it reports what a proxy with `opts` computes.
    pub async fn paths_with(&self, opts: &CmdOpts) -> PathsJson {
        let out = self
            .ferrex_cmd_with(CmdOpts {
                extra_args: vec!["daemon-paths".into()],
                ..opts.clone()
            })
            .stdout(Stdio::piped())
            .output()
            .await
            .expect("run daemon-paths");
        assert!(out.status.success(), "daemon-paths failed: {}", out.status);
        serde_json::from_slice(&out.stdout).expect("daemon-paths json")
    }
}

pub fn ferrex_bin() -> String {
    std::env::var("NEXTEST_BIN_EXE_ferrex")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_ferrex").to_string())
}

/// SIGKILLs the daemon on drop, so a panicking test leaves no process behind.
#[derive(Debug)]
pub struct DaemonGuard {
    pid: Option<u32>,
    pid_file: PathBuf,
}

impl DaemonGuard {
    pub fn for_child(child: &Child) -> Self {
        Self {
            pid: child.id(),
            pid_file: PathBuf::new(),
        }
    }

    /// For a daemon spawned by someone else: reads its PID once the PID file appears.
    pub async fn from_pid_file(pid_file: &Path, within: Duration) -> Self {
        wait_for_path(pid_file, within).await;
        let pid = read_pid_file(pid_file).map(|p| p.pid);
        Self {
            pid,
            pid_file: pid_file.to_path_buf(),
        }
    }

    pub const fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Forgets the PID once the process has been reaped, so `Drop` can't hit a recycled PID.
    pub const fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.filter(|&p| p > 1) {
            signal(pid, rustix::process::Signal::KILL);
        }
    }
}

pub fn signal(pid: u32, sig: rustix::process::Signal) {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    let _ = rustix::process::kill_process(pid, sig);
}

pub fn sigterm(child: &Child) {
    signal(
        child.id().expect("child pid"),
        rustix::process::Signal::TERM,
    );
}

pub fn read_pid_file(path: &Path) -> Option<PidFileJson> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub async fn wait_for_path(path: &Path, within: Duration) {
    let deadline = Instant::now() + within;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} did not appear within {within:?}",
            path.display()
        );
        tokio::time::sleep(POLL).await;
    }
}

pub async fn wait_exit(child: &mut Child, within: Duration) -> ExitStatus {
    tokio::time::timeout(within, child.wait())
        .await
        .unwrap_or_else(|_| panic!("process did not exit within {within:?}"))
        .expect("wait for child")
}

pub fn hello_line(key: &str, ns: &str) -> String {
    let mut line = json!({"ferrex_hello": 1, "key": key, "namespace": ns}).to_string();
    line.push('\n');
    line
}

pub async fn raw_session(
    socket: &Path,
    key: &str,
    ns: &str,
    hello_timeout: Duration,
) -> (HelloReplyJson, Option<McpConn>) {
    let stream = UnixStream::connect(socket).await.expect("connect daemon");
    let (read, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let exchange = async {
        writer.write_all(hello_line(key, ns).as_bytes()).await?;
        reader.read_line(&mut line).await
    };
    let n = tokio::time::timeout(hello_timeout, exchange)
        .await
        .unwrap_or_else(|_| panic!("no hello reply within {hello_timeout:?}"))
        .expect("hello exchange");
    assert!(n > 0, "daemon closed the connection before the hello reply");
    let reply: HelloReplyJson = serde_json::from_str(&line).expect("hello reply json");
    let conn = reply.ok.then_some(McpConn {
        reader,
        writer,
        next_id: 1,
    });
    (reply, conn)
}

pub struct McpConn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl McpConn {
    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            loop {
                let msg = self.read_message().await;
                if msg.get("id") == Some(&json!(id)) {
                    return msg;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no response to {method} within {REQUEST_TIMEOUT:?}"))
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    pub async fn initialize(&mut self) -> Value {
        let resp = self.request("initialize", initialize_params()).await;
        self.notify("notifications/initialized", json!({})).await;
        resp
    }

    pub async fn tool_names(&mut self) -> Vec<String> {
        let resp = self.request("tools/list", json!({})).await;
        resp["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("tools/list result: {resp}"))
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_owned))
            .collect()
    }

    async fn send(&mut self, msg: &Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .expect("write to daemon");
    }

    async fn read_message(&mut self) -> Value {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .expect("read from daemon");
        assert!(n > 0, "daemon closed the MCP session");
        serde_json::from_str(&line).expect("json-rpc line")
    }
}

pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "test", "version": "0.1.0"}
    })
}

pub fn request_line(id: u64, method: &str, params: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// What a fake-daemon connection does with its hello.
#[derive(Debug, Clone, Copy)]
pub enum FakeConn {
    Refuse(&'static str),
    CloseBeforeReply,
    NeverReply,
    Serve(FakeBehaviour),
    DelayHello(u64, FakeBehaviour),
}

/// What a fake-daemon connection does with requests after an ok hello.
#[derive(Debug, Clone, Copy)]
pub enum FakeBehaviour {
    Echo,
    /// Closes the connection when the k-th request (1-based) arrives, without answering it.
    CloseOnRequest(usize),
    NeverAnswer,
    DelayMs(u64),
}

/// A scripted stand-in for the daemon: connection n follows `script[n]`, the last entry repeats.
pub struct FakeDaemon {
    received: Arc<Mutex<Vec<Vec<String>>>>,
    accept: JoinHandle<()>,
}

impl FakeDaemon {
    pub fn start(socket: &Path, script: Vec<FakeConn>) -> Self {
        assert!(!script.is_empty(), "fake daemon needs a script");
        let listener = UnixListener::bind(socket).expect("bind fake daemon");
        let received = Arc::default();
        let accept = tokio::spawn(fake_accept_loop(listener, script, Arc::clone(&received)));
        Self { received, accept }
    }

    /// The lines each connection received after its hello, in accept order.
    pub fn received(&self) -> Vec<Vec<String>> {
        self.received.lock().unwrap().clone()
    }

    pub fn connections(&self) -> usize {
        self.received.lock().unwrap().len()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn fake_accept_loop(
    listener: UnixListener,
    script: Vec<FakeConn>,
    received: Arc<Mutex<Vec<Vec<String>>>>,
) {
    let mut conns = JoinSet::new();
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let idx = {
            let mut all = received.lock().unwrap();
            all.push(Vec::new());
            all.len() - 1
        };
        let entry = script[idx.min(script.len() - 1)];
        conns.spawn(fake_conn(stream, entry, idx, Arc::clone(&received)));
    }
}

async fn fake_conn(
    stream: UnixStream,
    entry: FakeConn,
    idx: usize,
    received: Arc<Mutex<Vec<Vec<String>>>>,
) {
    let (read, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut hello = Vec::new();
    if reader.read_until(b'\n', &mut hello).await.unwrap_or(0) == 0 {
        return;
    }
    let behaviour = match entry {
        FakeConn::Refuse(kind) => {
            let reply = json!({"ok": false, "kind": kind, "error": "fake daemon refused"});
            let _ = writer.write_all(format!("{reply}\n").as_bytes()).await;
            return;
        }
        FakeConn::CloseBeforeReply => return,
        FakeConn::NeverReply => {
            std::future::pending::<()>().await;
            return;
        }
        FakeConn::Serve(behaviour) => behaviour,
        FakeConn::DelayHello(ms, behaviour) => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            behaviour
        }
    };
    if writer.write_all(b"{\"ok\":true}\n").await.is_err() {
        return;
    }
    let mut requests = 0;
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        received.lock().unwrap()[idx].push(String::from_utf8_lossy(text).into_owned());
        let Some(reply) = echo_reply(&line) else {
            continue;
        };
        requests += 1;
        match behaviour {
            FakeBehaviour::CloseOnRequest(k) if requests >= k => return,
            FakeBehaviour::Echo | FakeBehaviour::CloseOnRequest(_) => {}
            FakeBehaviour::NeverAnswer => continue,
            FakeBehaviour::DelayMs(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
        }
        if writer.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn echo_reply(line: &[u8]) -> Option<String> {
    let msg: Value = serde_json::from_slice(line).ok()?;
    let id = msg.get("id")?;
    let method = msg.get("method")?.as_str()?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let reply = json!({"jsonrpc": "2.0", "id": id, "result": {"echo": method, "params": params}});
    Some(format!("{reply}\n"))
}

/// A proxy (`ferrex` with no subcommand) driven over its stdio, with stderr captured.
pub struct ProxyClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_drain: Option<JoinHandle<()>>,
    next_id: u64,
    seen: Vec<Value>,
}

impl ProxyClient {
    pub fn spawn(env: &TestEnv, opts: CmdOpts) -> Self {
        let mut cmd = env.ferrex_cmd_with(opts);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn proxy");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("proxy stdout"));
        let mut pipe = child.stderr.take().expect("proxy stderr");
        let stderr = Arc::<Mutex<Vec<u8>>>::default();
        let sink = Arc::clone(&stderr);
        let stderr_drain = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = pipe.read(&mut buf).await {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self {
            child,
            stdin,
            stdout,
            stderr,
            stderr_drain: Some(stderr_drain),
            next_id: 1,
            seen: Vec::new(),
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id().expect("proxy pid")
    }

    /// `true` while the proxy process has not exited.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Every message `response_within` has read from stdout, in order.
    pub fn seen(&self) -> &[Value] {
        &self.seen
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }

    /// Writes `line`, adding the newline if it is missing.
    pub async fn send(&mut self, line: impl AsRef<[u8]>) {
        let line = line.as_ref();
        let stdin = self.stdin.as_mut().expect("proxy stdin is open");
        stdin.write_all(line).await.expect("write proxy stdin");
        if !line.ends_with(b"\n") {
            stdin.write_all(b"\n").await.expect("write proxy stdin");
        }
        stdin.flush().await.expect("flush proxy stdin");
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string())
            .await;
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        self.request_within(method, params, REQUEST_TIMEOUT).await
    }

    pub async fn request_within(&mut self, method: &str, params: Value, within: Duration) -> Value {
        let id = self.send_request(method, &params).await;
        self.response_within(id, within).await
    }

    /// Sends a request without waiting for its response; returns its id.
    pub async fn send_request(&mut self, method: &str, params: &Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(request_line(id, method, params)).await;
        id
    }

    /// Reads lines until the response to `id` arrives.
    pub async fn response_within(&mut self, id: u64, within: Duration) -> Value {
        let deadline = Instant::now() + within;
        loop {
            let line = self
                .read_line_timeout(deadline.saturating_duration_since(Instant::now()))
                .await
                .unwrap_or_else(|| {
                    panic!(
                        "no response to id {id} within {within:?}; stderr:\n{}",
                        self.stderr_text()
                    )
                });
            let msg: Value = serde_json::from_str(&line).expect("json-rpc line");
            self.seen.push(msg.clone());
            if msg.get("id") == Some(&json!(id)) {
                return msg;
            }
        }
    }

    pub async fn initialize(&mut self) -> Value {
        let resp = self.request("initialize", initialize_params()).await;
        self.notify("notifications/initialized", json!({})).await;
        resp
    }

    /// Reads lines until a response to each of `ids` has arrived in any order; returns them in `ids` order.
    pub async fn responses_within(&mut self, ids: &[u64], within: Duration) -> Vec<Value> {
        let deadline = Instant::now() + within;
        let mut found: Vec<Option<Value>> = vec![None; ids.len()];
        while found.iter().any(Option::is_none) {
            let line = self
                .read_line_timeout(deadline.saturating_duration_since(Instant::now()))
                .await
                .unwrap_or_else(|| {
                    panic!(
                        "no responses to all of {ids:?} within {within:?}; stderr:\n{}",
                        self.stderr_text()
                    )
                });
            let msg: Value = serde_json::from_str(&line).expect("json-rpc line");
            self.seen.push(msg.clone());
            if let Some(slot) = ids.iter().position(|id| msg.get("id") == Some(&json!(id))) {
                found[slot] = Some(msg);
            }
        }
        found.into_iter().flatten().collect()
    }

    /// `None` on timeout or EOF.
    pub async fn read_line_timeout(&mut self, within: Duration) -> Option<String> {
        let mut line = String::new();
        match tokio::time::timeout(within, self.stdout.read_line(&mut line)).await {
            Ok(Ok(n)) if n > 0 => Some(line),
            _ => None,
        }
    }

    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Waits for the exit and for stderr to drain, so `stderr_text` is complete afterwards.
    pub async fn wait_exit(&mut self, within: Duration) -> Option<ExitStatus> {
        let status = tokio::time::timeout(within, self.child.wait())
            .await
            .ok()?
            .expect("wait for proxy");
        if let Some(drain) = self.stderr_drain.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), drain).await;
        }
        Some(status)
    }
}
