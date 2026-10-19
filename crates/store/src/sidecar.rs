use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::Pid;

use crate::StoreError;

const HEALTH_CHECK_CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_CHECK_INITIAL_DELAY: Duration = Duration::from_millis(200);
const HEALTH_CHECK_MAX_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_CHECK_MAX_DELAY: Duration = Duration::from_secs(2);
const MIN_PORT: u16 = 2;
const COMM_MATCH_LEN: usize = 15;
const PID_FILE: &str = "qdrant.pid";
const PORT_FILE: &str = "qdrant.port";
const LOCK_FILE: &str = "qdrant.lock";

#[derive(Debug)]
pub struct QdrantSidecar {
    dir: PathBuf,
    process: Option<Child>,
    port: u16,
    cached_pid: Option<u32>,
}

enum Resolution {
    Reuse(u32),
    Spawn(SidecarLock),
}

impl QdrantSidecar {
    pub async fn start(
        bin: &str,
        port: u16,
        base_dir: Option<PathBuf>,
    ) -> Result<Self, StoreError> {
        let dir = base_dir.unwrap_or_else(|| dirs_home().join(".ferrex"));
        fs::create_dir_all(&dir)
            .map_err(|e| StoreError::Sidecar(format!("failed to create ~/.ferrex: {e}")))?;

        match resolve_existing(bin, port, &dir).await? {
            Resolution::Reuse(pid) => {
                tracing::info!(pid, "reusing existing Qdrant process");
                health_check(port, None).await?;
                Ok(Self {
                    dir,
                    process: None,
                    port,
                    cached_pid: Some(pid),
                })
            }
            Resolution::Spawn(lock) => Self::spawn(bin, port, dir, lock).await,
        }
    }

    async fn spawn(
        bin: &str,
        port: u16,
        dir: PathBuf,
        lock: SidecarLock,
    ) -> Result<Self, StoreError> {
        if port < MIN_PORT {
            return Err(StoreError::Sidecar(format!(
                "port must be >= {MIN_PORT} (got {port}), http_port is port - 1"
            )));
        }

        let data_dir = dir.join("qdrant-data");
        let snapshots_dir = dir.join("qdrant-snapshots");
        let config_path = dir.join("qdrant-config.yaml");
        fs::create_dir_all(&data_dir)
            .map_err(|e| StoreError::Sidecar(format!("failed to create qdrant-data: {e}")))?;
        let http_port = port - 1;
        let config = format!(
            "storage:\n  storage_path: {}\n  snapshots_path: {}\nservice:\n  grpc_port: {port}\n  http_port: {http_port}\n",
            data_dir.display(),
            snapshots_dir.display()
        );
        fs::write(&config_path, config)
            .map_err(|e| StoreError::Sidecar(format!("failed to write qdrant config: {e}")))?;

        tracing::info!(bin = bin, port = port, "starting Qdrant sidecar");
        let mut child = Command::new(bin)
            .arg("--config-path")
            .arg(&config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| StoreError::Sidecar(format!("failed to spawn qdrant: {e}")))?;

        let pid = child.id();
        if let Err(e) = write_pid_files(&dir, pid, port) {
            drop(lock);
            reap(&mut child, &dir);
            return Err(e);
        }
        drop(lock);

        if let Err(e) = health_check(port, Some(&mut child)).await {
            reap(&mut child, &dir);
            return Err(e);
        }
        Ok(Self {
            dir,
            process: Some(child),
            port,
            cached_pid: Some(pid),
        })
    }

    #[must_use]
    pub const fn pid(&self) -> Option<u32> {
        self.cached_pid
    }

    #[must_use]
    pub fn url(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    pub fn shutdown(&mut self) {
        if let Some(mut child) = self.process.take() {
            reap(&mut child, &self.dir);
            tracing::info!("Qdrant sidecar stopped");
        }
    }
}

impl Drop for QdrantSidecar {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn resolve_existing(bin: &str, port: u16, dir: &Path) -> Result<Resolution, StoreError> {
    let pid_file = dir.join(PID_FILE);
    let lock_path = dir.join(LOCK_FILE);
    // Prevents two processes from racing to spawn qdrant.
    let mut lock = acquire_lock_file(&lock_path)?;

    loop {
        let Some(existing) = read_pid(&pid_file) else {
            return Ok(Resolution::Spawn(lock));
        };
        if !pid_alive(existing) {
            tracing::info!(pid = existing, "cleaning stale PID file");
            remove_pid_files(dir);
            return Ok(Resolution::Spawn(lock));
        }

        let recorded = recorded_port(dir, existing);
        if is_qdrant(existing, bin) {
            check_recorded_port(existing, recorded, port)?;
            return Ok(Resolution::Reuse(existing));
        }

        // The name may just be a wrapper script's, so ask the port. Other starters
        // give up on the lock quickly, so don't hold it across the probe.
        drop(lock);
        let probe_port = recorded.unwrap_or(port);
        let answers = if pid_file_is_fresh(&pid_file) {
            health_check(probe_port, None).await.is_ok()
        } else {
            probe_qdrant(probe_port).await
        };
        if answers {
            check_recorded_port(existing, recorded, port)?;
            return Ok(Resolution::Reuse(existing));
        }

        lock = acquire_lock_file(&lock_path)?;
        if read_pid(&pid_file) == Some(existing) {
            tracing::info!(
                pid = existing,
                "PID file names a non-qdrant process, replacing"
            );
            remove_pid_files(dir);
            return Ok(Resolution::Spawn(lock));
        }
    }
}

fn recorded_port(dir: &Path, pid: u32) -> Option<u16> {
    read_port_file(&dir.join(PORT_FILE))
        .filter(|&(recorded_pid, _)| recorded_pid == pid)
        .map(|(_, port)| port)
}

fn check_recorded_port(pid: u32, recorded: Option<u16>, port: u16) -> Result<(), StoreError> {
    match recorded {
        Some(recorded) if recorded != port => Err(StoreError::Sidecar(format!(
            "qdrant sidecar (pid {pid}) is running on port {recorded}, but qdrant_port is {port}. \
             Set qdrant_port = {recorded}, or close the sessions still using the old config \
             (or kill their ferrex daemons), then stop qdrant"
        ))),
        _ => Ok(()),
    }
}

fn pid_file_is_fresh(pid_file: &Path) -> bool {
    fs::metadata(pid_file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|age| age < HEALTH_CHECK_MAX_TIMEOUT)
}

fn write_pid_files(dir: &Path, pid: u32, port: u16) -> Result<(), StoreError> {
    fs::write(dir.join(PID_FILE), pid.to_string())
        .map_err(|e| StoreError::Sidecar(format!("failed to write PID file: {e}")))?;
    fs::write(dir.join(PORT_FILE), format!("{pid} {port}"))
        .map_err(|e| StoreError::Sidecar(format!("failed to write port file: {e}")))
}

fn remove_pid_files(dir: &Path) {
    let _ = fs::remove_file(dir.join(PID_FILE));
    let _ = fs::remove_file(dir.join(PORT_FILE));
}

fn reap(child: &mut Child, dir: &Path) {
    let _ = child.kill();
    let _ = child.wait();
    remove_pid_files_if_ours(dir, child.id());
}

fn remove_pid_files_if_ours(dir: &Path, pid: u32) {
    let Ok(_lock) = acquire_lock_file(&dir.join(LOCK_FILE)) else {
        tracing::warn!(
            pid,
            "sidecar lock busy, leaving PID files for the next start"
        );
        return;
    };
    if read_pid(&dir.join(PID_FILE)) == Some(pid) {
        remove_pid_files(dir);
    }
}

fn build_client(port: u16) -> Result<qdrant_client::Qdrant, StoreError> {
    qdrant_client::Qdrant::from_url(&format!("http://localhost:{port}"))
        .timeout(HEALTH_CHECK_CLIENT_TIMEOUT)
        .skip_compatibility_check()
        .build()
        .map_err(|e| StoreError::Sidecar(e.to_string()))
}

/// One-shot check that something answers qdrant's health endpoint on `port`.
pub async fn probe_qdrant(port: u16) -> bool {
    match build_client(port) {
        Ok(client) => client.health_check().await.is_ok(),
        Err(_) => false,
    }
}

fn ensure_running(child: Option<&mut Child>) -> Result<(), StoreError> {
    match child.map(Child::try_wait) {
        Some(Ok(Some(status))) => Err(StoreError::Sidecar(format!(
            "qdrant exited during health check ({status})"
        ))),
        _ => Ok(()),
    }
}

async fn health_check(port: u16, mut child: Option<&mut Child>) -> Result<(), StoreError> {
    let client = build_client(port)?;
    if child.is_some() {
        tokio::time::sleep(HEALTH_CHECK_INITIAL_DELAY).await;
    }

    let mut delay = HEALTH_CHECK_INITIAL_DELAY;
    let start = Instant::now();

    loop {
        ensure_running(child.as_deref_mut())?;
        match client.health_check().await {
            Ok(_) => {
                // Someone else's qdrant may be answering for a child that just died.
                ensure_running(child.as_deref_mut())?;
                tracing::info!("Qdrant sidecar healthy");
                return Ok(());
            }
            Err(e) => {
                if start.elapsed() > HEALTH_CHECK_MAX_TIMEOUT {
                    return Err(StoreError::Sidecar(format!(
                        "Qdrant health check timed out after {HEALTH_CHECK_MAX_TIMEOUT:?}: {e}"
                    )));
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(HEALTH_CHECK_MAX_DELAY);
            }
        }
    }
}

fn dirs_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug)]
struct SidecarLock {
    path: PathBuf,
    _file: File,
}

impl Drop for SidecarLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_lock_file(path: &Path) -> Result<SidecarLock, StoreError> {
    const MAX_ATTEMPTS: u32 = 10;
    const RETRY_DELAY: Duration = Duration::from_millis(200);
    const STALE_LOCK_AGE: Duration = Duration::from_secs(30);

    for attempt in 0..MAX_ATTEMPTS {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => {
                return Ok(SidecarLock {
                    path: path.to_path_buf(),
                    _file: file,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Ok(meta) = fs::metadata(path)
                    && let Some(age) = meta.modified().ok().and_then(|m| m.elapsed().ok())
                    && age > STALE_LOCK_AGE
                {
                    tracing::warn!("removing stale lock file");
                    let _ = fs::remove_file(path);
                    continue;
                }
                if attempt + 1 < MAX_ATTEMPTS {
                    std::thread::sleep(RETRY_DELAY);
                }
            }
            Err(e) => {
                return Err(StoreError::Sidecar(format!(
                    "failed to create lock file: {e}"
                )));
            }
        }
    }
    Err(StoreError::Sidecar(
        "timed out waiting for sidecar lock file".to_string(),
    ))
}

/// Reads a bare PID file; `None` if missing or malformed.
#[must_use]
pub fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Reads a `<pid> <port>` file; `None` if missing or malformed.
#[must_use]
pub fn read_port_file(path: &Path) -> Option<(u32, u16)> {
    let content = fs::read_to_string(path).ok()?;
    let (pid, port) = content.trim().split_once(' ')?;
    Some((pid.parse().ok()?, port.trim().parse().ok()?))
}

/// Whether `pid` names a live, non-zombie process.
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    let Some(raw) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    match rustix::process::test_kill_process(raw) {
        Ok(()) => !is_zombie(pid),
        Err(Errno::PERM) => true,
        Err(_) => false,
    }
}

fn is_zombie(pid: u32) -> bool {
    process_state(pid).is_some_and(|state| state.starts_with('Z'))
}

#[cfg(target_os = "linux")]
fn process_state(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    Some(after_comm.trim_start().to_string()).filter(|s| !s.is_empty())
}

#[cfg(not(target_os = "linux"))]
fn process_state(pid: u32) -> Option<String> {
    ps_field("stat", pid)
}

#[cfg(target_os = "linux")]
fn process_comm(pid: u32) -> Option<String> {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(not(target_os = "linux"))]
fn process_comm(pid: u32) -> Option<String> {
    ps_field("comm", pid)
}

// Absolute path: a nix build's PATH has no ps.
#[cfg(not(target_os = "linux"))]
fn ps_field(field: &str, pid: u32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-o", &format!("{field}="), "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

/// Whether `pid` is a process whose name matches `qdrant_bin`.
#[must_use]
pub fn is_qdrant(pid: u32, qdrant_bin: &str) -> bool {
    process_comm(pid).is_some_and(|comm| matches_qdrant_name(&comm, qdrant_bin))
}

fn matches_qdrant_name(comm: &str, qdrant_bin: &str) -> bool {
    fn basename(path: &str) -> &str {
        Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path)
    }
    fn prefix(name: &str) -> impl Iterator<Item = char> {
        name.chars().take(COMM_MATCH_LEN)
    }

    let name = basename(comm);
    let name = name.strip_prefix('.').unwrap_or(name);
    let name = name.strip_suffix("-wrapped").unwrap_or(name);
    prefix(name).eq(prefix(basename(qdrant_bin)))
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Instant, SystemTime};

    use rustix::io::Errno;
    use rustix::process::{Pid, Signal};

    use super::*;

    // Nextest runs each test in its own process, so claim ports via marker files
    // to keep concurrent qdrant tests off each other's gRPC/HTTP pair.
    fn claim(port: u16) -> bool {
        let dir = std::env::temp_dir().join("ferrex-test-ports");
        let _ = fs::create_dir_all(&dir);
        let marker = dir.join(port.to_string());
        let stale = fs::metadata(&marker)
            .and_then(|m| m.modified())
            .is_ok_and(|m| m.elapsed().is_ok_and(|e| e > Duration::from_secs(600)));
        if stale {
            let _ = fs::remove_file(&marker);
        }
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .is_ok()
    }

    fn free_port() -> u16 {
        loop {
            if let Some(p) = portpicker::pick_unused_port()
                && p >= MIN_PORT
                && portpicker::is_free(p - 1)
                && claim(p)
                && claim(p - 1)
            {
                return p;
            }
        }
    }

    fn write_script(path: &Path, body: &str) {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(path)
            .unwrap();
        std::io::Write::write_all(&mut f, body.as_bytes()).unwrap();
    }

    fn exit_script(dir: &Path) -> PathBuf {
        let script = dir.join("fake-qdrant.sh");
        write_script(
            &script,
            "#!/bin/sh\necho $$ > \"$(dirname \"$0\")/child.pid\"\nexit 1\n",
        );
        script
    }

    // Retries when a concurrent fork holds the script open for writing (rust-lang/rust#114554).
    async fn start_retrying_txtbsy(
        bin: &str,
        port: u16,
        dir: &Path,
    ) -> Result<QdrantSidecar, StoreError> {
        let marker = format!("os error {}", Errno::TXTBSY.raw_os_error());
        let mut result = QdrantSidecar::start(bin, port, Some(dir.to_path_buf())).await;
        for _ in 0..2 {
            match &result {
                Err(StoreError::Sidecar(msg)) if msg.contains(&marker) => {
                    result = QdrantSidecar::start(bin, port, Some(dir.to_path_buf())).await;
                }
                _ => break,
            }
        }
        result
    }

    fn kill_pid(pid: u32) {
        if let Some(p) = i32::try_from(pid).ok().and_then(Pid::from_raw) {
            let _ = rustix::process::kill_process(p, Signal::KILL);
        }
    }

    #[tokio::test]
    async fn start_error_under_lock_releases_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        assert!(
            QdrantSidecar::start("qdrant", 1, Some(base.clone()))
                .await
                .is_err()
        );
        let err = QdrantSidecar::start("qdrant", 1, Some(base))
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(!err.contains("timed out waiting for sidecar lock"), "{err}");
    }

    #[test]
    fn pid_alive_true_for_self() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_rejects_zero_and_out_of_range() {
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
    }

    #[test]
    fn pid_alive_false_for_unreaped_zombie() {
        if cfg!(target_os = "macos") && !Path::new("/bin/ps").exists() {
            eprintln!("skipping: /bin/ps missing");
            return;
        }
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!pid_alive(child.id()));
        child.wait().unwrap();
    }

    #[test]
    fn matches_qdrant_name_accepts_variants() {
        for bin in ["qdrant", "/nix/store/abc/bin/qdrant"] {
            for comm in [
                "qdrant",
                "/nix/store/xyz-qdrant-1.13/bin/qdrant",
                ".qdrant-wrapped",
            ] {
                assert!(matches_qdrant_name(comm, bin), "{comm} vs {bin}");
            }
            for comm in ["sh", "sleep", "qdrantx-other"] {
                assert!(!matches_qdrant_name(comm, bin), "{comm} vs {bin}");
            }
        }
    }

    #[test]
    fn is_qdrant_false_when_name_unreadable() {
        assert!(!is_qdrant(u32::MAX - 1, "qdrant"));
    }

    #[tokio::test]
    async fn start_fails_and_cleans_up_when_bin_exits_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let script = exit_script(dir.path());
        let started = Instant::now();
        let result = start_retrying_txtbsy(script.to_str().unwrap(), free_port(), dir.path()).await;
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!dir.path().join("qdrant.pid").exists());
        assert!(!dir.path().join("qdrant.port").exists());
        let pid = read_pid(&dir.path().join("child.pid")).unwrap();
        let pid = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
        assert_eq!(rustix::process::test_kill_process(pid), Err(Errno::SRCH));
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn test_sidecar_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let mut sidecar = QdrantSidecar::start("qdrant", port, Some(dir.path().to_path_buf()))
            .await
            .unwrap();
        assert!(sidecar.url().contains(&port.to_string()));
        sidecar.shutdown();
        assert!(!dir.path().join("qdrant.pid").exists());
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn port_file_mismatch_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let port_a = free_port();
        let first = QdrantSidecar::start("qdrant", port_a, Some(base.clone()))
            .await
            .unwrap();
        let pid = first.pid().unwrap();
        assert_eq!(
            read_port_file(&dir.path().join("qdrant.port")),
            Some((pid, port_a))
        );
        let port_b = free_port();
        let started = Instant::now();
        let err = QdrantSidecar::start("qdrant", port_b, Some(base))
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(err.contains("is running on port"), "{err}");
        assert!(err.contains("but qdrant_port is"), "{err}");
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn missing_port_file_skips_check() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let port = free_port();
        let first = QdrantSidecar::start("qdrant", port, Some(base.clone()))
            .await
            .unwrap();
        fs::remove_file(dir.path().join("qdrant.port")).unwrap();
        let second = QdrantSidecar::start("qdrant", port, Some(base))
            .await
            .unwrap();
        assert_eq!(first.pid(), second.pid());
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn stale_pid_of_non_qdrant_process_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let mut sleeper = Command::new("sleep").arg("60").spawn().unwrap();
        let pid_file = dir.path().join("qdrant.pid");
        fs::write(&pid_file, sleeper.id().to_string()).unwrap();
        File::options()
            .write(true)
            .open(&pid_file)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let result =
            QdrantSidecar::start("qdrant", free_port(), Some(dir.path().to_path_buf())).await;
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        let sidecar = result.unwrap();
        assert_ne!(sidecar.pid(), Some(sleeper.id()));
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn fresh_pid_file_of_non_qdrant_gets_full_health_check() {
        let dir = tempfile::tempdir().unwrap();
        let mut sleeper = Command::new("sleep").arg("60").spawn().unwrap();
        fs::write(dir.path().join("qdrant.pid"), sleeper.id().to_string()).unwrap();
        let started = Instant::now();
        let result =
            QdrantSidecar::start("qdrant", free_port(), Some(dir.path().to_path_buf())).await;
        let elapsed = started.elapsed();
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        let sidecar = result.unwrap();
        assert!(elapsed >= Duration::from_secs(25), "{elapsed:?}");
        assert_ne!(sidecar.pid(), Some(sleeper.id()));
    }

    struct KillInner(PathBuf);

    impl Drop for KillInner {
        fn drop(&mut self) {
            if let Some(pid) = read_pid(&self.0) {
                kill_pid(pid);
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn wrapper_script_qdrant_is_reused() {
        for (name, body, has_inner) in [
            ("exec", "#!/bin/sh\nexec qdrant \"$@\"\n", false),
            (
                "non-exec",
                "#!/bin/sh\nqdrant \"$@\" &\necho $! > \"$(dirname \"$0\")/inner.pid\"\nwait\n",
                true,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let _guard = has_inner.then(|| KillInner(dir.path().join("inner.pid")));
            let script = dir.path().join("wrapper.sh");
            write_script(&script, body);
            let port = free_port();
            let first = start_retrying_txtbsy(script.to_str().unwrap(), port, dir.path())
                .await
                .unwrap();
            let second = QdrantSidecar::start("qdrant", port, Some(dir.path().to_path_buf()))
                .await
                .unwrap();
            assert_eq!(first.pid(), second.pid(), "{name}");
        }
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn killondrop_shutdown_leaves_foreign_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let mut sidecar = QdrantSidecar::start("qdrant", port, Some(dir.path().to_path_buf()))
            .await
            .unwrap();
        let mut sleeper = Command::new("sleep").arg("60").spawn().unwrap();
        let foreign = sleeper.id();
        fs::write(dir.path().join("qdrant.pid"), foreign.to_string()).unwrap();
        fs::write(dir.path().join("qdrant.port"), format!("{foreign} {port}")).unwrap();
        sidecar.shutdown();
        let pid_content = fs::read_to_string(dir.path().join("qdrant.pid"));
        let port_content = fs::read_to_string(dir.path().join("qdrant.port"));
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        assert_eq!(pid_content.unwrap(), foreign.to_string());
        assert_eq!(port_content.unwrap(), format!("{foreign} {port}"));
    }

    #[tokio::test]
    #[ignore = "requires qdrant binary on PATH"]
    async fn bin_exits_while_real_qdrant_answers_fails() {
        let real = tempfile::tempdir().unwrap();
        let port = free_port();
        let _qdrant = QdrantSidecar::start("qdrant", port, Some(real.path().to_path_buf()))
            .await
            .unwrap();
        let fake = tempfile::tempdir().unwrap();
        let script = exit_script(fake.path());
        let result = start_retrying_txtbsy(script.to_str().unwrap(), port, fake.path()).await;
        assert!(result.is_err());
    }
}
