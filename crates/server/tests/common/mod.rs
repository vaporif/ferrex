#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use ferrex_store::{QdrantSidecar, SidecarPolicy};
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, Command};
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
        let mut cmd = Command::new(ferrex_bin());
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
        let out = self
            .ferrex_cmd_with(CmdOpts {
                extra_args: vec!["daemon-paths".into()],
                ..Default::default()
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
        let resp = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0.1.0"}
                }),
            )
            .await;
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
