use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use ferrex_core::FerrexConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

#[cfg(target_os = "macos")]
pub const MAX_SUN_PATH: usize = 103;
#[cfg(not(target_os = "macos"))]
pub const MAX_SUN_PATH: usize = 107;

const LOCK_POLL: Duration = Duration::from_millis(50);

/// Opens `path` and polls for an exclusive flock until `deadline`; `Ok(None)` on timeout.
pub async fn try_lock_until(path: &Path, deadline: Instant) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(LOCK_POLL).await;
    }
}

#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub run_dir: PathBuf,
    pub key: String,
    pub socket: PathBuf,
    pub pid_file: PathBuf,
    pub lock_file: PathBuf,
    pub log_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidFile {
    pub pid: u32,
    pub db_path: PathBuf,
}

/// Resolves the run dir; a relative `FERREX_RUN_DIR` is anchored to the current directory so every proxy agrees.
pub fn run_dir() -> eyre::Result<PathBuf> {
    let Some(dir) = std::env::var_os("FERREX_RUN_DIR").map(PathBuf::from) else {
        return Ok(dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".ferrex")
            .join("run"));
    };
    if dir.is_absolute() {
        return Ok(dir);
    }
    let cwd = std::env::current_dir().map_err(|e| {
        eyre::eyre!(
            "FERREX_RUN_DIR {} is relative and the current directory is unavailable: {e}",
            dir.display()
        )
    })?;
    Ok(cwd.join(dir))
}

/// Creates the run dir private to this user; an existing one must already be private, since
/// anyone who can reach its sockets can talk to the daemon.
pub fn ensure_run_dir(dir: &Path) -> eyre::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| eyre::eyre!("create run dir {}: {e}", dir.display()))?;
    let meta =
        std::fs::metadata(dir).map_err(|e| eyre::eyre!("stat run dir {}: {e}", dir.display()))?;
    let uid = rustix::process::geteuid().as_raw();
    if meta.uid() != uid {
        eyre::bail!(
            "run dir {} is owned by uid {}, not {uid}; point FERREX_RUN_DIR at a directory of your own",
            dir.display(),
            meta.uid()
        );
    }
    if meta.mode() & 0o077 != 0 {
        eyre::bail!(
            "run dir {} is accessible to other users (mode {:o}); run `chmod 700 {}` or point FERREX_RUN_DIR elsewhere",
            dir.display(),
            meta.mode() & 0o777,
            dir.display()
        );
    }
    Ok(())
}

pub fn current_exe_canonical() -> eyre::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.canonicalize()
        .map_err(|e| eyre::eyre!("resolve {}: {e}", exe.display()))
}

pub fn compute_key(config: &FerrexConfig, exe: &Path) -> eyre::Result<String> {
    let mtime = std::fs::metadata(exe)?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_nanos();
    let mut value = serde_json::to_value(config)?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("config is not an object"))?;
    obj.remove("namespace");
    if config.qdrant_url.is_none()
        && let Some(resolved) = resolve_on_path(&config.qdrant_bin)
    {
        obj.insert(
            "qdrant_bin".into(),
            serde_json::Value::String(resolved.display().to_string()),
        );
    }
    let mut hasher = Sha256::new();
    hasher.update(exe.as_os_str().as_bytes());
    hasher.update([0]);
    hasher.update(mtime.to_le_bytes());
    hasher.update(serde_json::to_vec(&value)?);
    Ok(hex16(&hasher.finalize()))
}

fn hex16(digest: &[u8]) -> String {
    use std::fmt::Write;

    digest
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn resolve_on_path(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        return Path::new(bin).canonicalize().ok();
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(bin))
        .find(|candidate| is_executable_file(candidate))
        .and_then(|found| found.canonicalize().ok())
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl DaemonPaths {
    pub fn for_config(config: &FerrexConfig, exe: &Path) -> eyre::Result<Self> {
        let run_dir = run_dir()?;
        let key = compute_key(config, exe)?;
        let socket = run_dir.join(format!("{key}.sock"));
        let len = socket.as_os_str().len();
        if len > MAX_SUN_PATH {
            eyre::bail!(
                "socket path is {len} bytes, over the {MAX_SUN_PATH} limit: {} (set FERREX_RUN_DIR to a shorter directory)",
                socket.display()
            );
        }
        Ok(Self {
            pid_file: run_dir.join(format!("{key}.pid")),
            lock_file: run_dir.join(format!("{key}.lock")),
            log_file: run_dir.join("ferrexd.log"),
            run_dir,
            key,
            socket,
        })
    }
}

impl PidFile {
    /// An unlocked PID file, as a crashed daemon leaves behind.
    #[cfg(test)]
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let tmp = Self::tmp_path(path);
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)
    }

    /// Writes the file under an exclusive lock that marks the daemon alive until the returned
    /// handle closes; the lock stays on the file through the rename.
    pub fn write_locked(&self, path: &Path) -> io::Result<File> {
        let tmp = Self::tmp_path(path);
        let mut file = File::create(&tmp)?;
        file.try_lock().map_err(io::Error::from)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(file)
    }

    fn tmp_path(path: &Path) -> PathBuf {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        PathBuf::from(tmp)
    }

    #[must_use]
    pub fn read(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path)
            .inspect_err(
                |e| tracing::debug!(path = %path.display(), error = %e, "pid file unreadable"),
            )
            .ok()?;
        serde_json::from_slice(&bytes)
            .inspect_err(
                |e| tracing::debug!(path = %path.display(), error = %e, "pid file unparsable"),
            )
            .ok()
    }
}

/// Whether a live daemon holds the lock on this PID file. Unlike connecting to its socket, this
/// doesn't count as a connection, and the kernel drops the lock when the daemon dies.
#[must_use]
pub fn pid_file_held(path: &Path) -> bool {
    File::open(path)
        .is_ok_and(|file| matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock)))
}

#[must_use]
pub fn live_for_db(run_dir: &Path, db_path: &Path) -> Option<u32> {
    live_daemons(run_dir)
        .find(|pid_file| pid_file.db_path == db_path)
        .map(|pid_file| pid_file.pid)
}

#[must_use]
pub fn any_live(run_dir: &Path) -> Option<u32> {
    live_daemons(run_dir).next().map(|pid_file| pid_file.pid)
}

/// Best-effort: PID files that are unreadable or not held by a live daemon are skipped.
fn live_daemons(run_dir: &Path) -> impl Iterator<Item = PidFile> {
    std::fs::read_dir(run_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pid"))
        .filter(|path| pid_file_held(path))
        .filter_map(|path| PidFile::read(&path))
}

#[cfg(test)]
pub(super) mod tests {
    use std::fs::{self, File};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use ferrex_core::{
        CacheConfig, ConflictConfig, DedupConfig, FerrexConfig, ModelTier, PredicatesConfig,
        ReconciliationConfig, RerankerTier, StalenessConfig,
    };

    use super::*;

    type Mutation = fn(&mut FerrexConfig);

    pub(in crate::daemon) fn base_config() -> FerrexConfig {
        FerrexConfig {
            qdrant_url: None,
            qdrant_bin: "qdrant".into(),
            qdrant_port: 6334,
            model_tier: ModelTier::Best,
            reranker_tier: RerankerTier::Default,
            namespace: "default".into(),
            db_path: PathBuf::from("/data/ferrex.db"),
            config_path: Some(PathBuf::from("/data/ferrex.toml")),
            deduplication: DedupConfig::default(),
            conflict: ConflictConfig::default(),
            predicates: PredicatesConfig::default(),
            reconciliation: ReconciliationConfig::default(),
            staleness: StalenessConfig::default(),
            reader_pool_size: 4,
            cache: CacheConfig::default(),
        }
    }

    fn exe() -> tempfile::NamedTempFile {
        tempfile::NamedTempFile::new().unwrap()
    }

    #[test]
    fn key_covers_every_field() {
        let FerrexConfig {
            qdrant_url: _,
            qdrant_bin: _,
            qdrant_port: _,
            model_tier: _,
            reranker_tier: _,
            namespace: _,
            db_path: _,
            config_path: _,
            deduplication: _,
            conflict: _,
            predicates: _,
            reconciliation: _,
            staleness: _,
            reader_pool_size: _,
            cache: _,
        } = base_config();
    }

    #[test]
    fn each_field_changes_key() {
        let exe = exe();
        let base = compute_key(&base_config(), exe.path()).unwrap();
        let cases: Vec<(&str, Mutation)> = vec![
            ("qdrant_url", |c| c.qdrant_url = Some("http://x:1".into())),
            ("qdrant_bin", |c| c.qdrant_bin = "other-qdrant".into()),
            ("qdrant_port", |c| c.qdrant_port = 7000),
            ("model_tier", |c| c.model_tier = ModelTier::Small),
            ("reranker_tier", |c| {
                c.reranker_tier = RerankerTier::Multilingual;
            }),
            ("db_path", |c| c.db_path = PathBuf::from("/data/other.db")),
            ("config_path", |c| c.config_path = None),
            ("deduplication", |c| c.deduplication.threshold = 0.5),
            ("conflict", |c| c.conflict.object_fuzzy_update = 0.1),
            ("predicates.groups", |c| {
                c.predicates.groups.insert("g".into(), vec!["a".into()]);
            }),
            ("reconciliation", |c| c.reconciliation.audit_fix_limit = 1),
            ("staleness.weights.age", |c| c.staleness.weights.age += 0.25),
            ("reader_pool_size", |c| c.reader_pool_size = 9),
            ("cache.result_capacity", |c| c.cache.result_capacity += 1),
        ];
        for (name, mutate) in cases {
            let mut config = base_config();
            mutate(&mut config);
            let key = compute_key(&config, exe.path()).unwrap();
            assert_ne!(key, base, "{name} did not change the key");
        }
    }

    #[test]
    fn namespace_does_not_change_key() {
        let exe = exe();
        let mut other = base_config();
        other.namespace = "elsewhere".into();
        assert_eq!(
            compute_key(&base_config(), exe.path()).unwrap(),
            compute_key(&other, exe.path()).unwrap()
        );
    }

    #[test]
    fn same_inputs_same_key() {
        let exe = exe();
        assert_eq!(
            compute_key(&base_config(), exe.path()).unwrap(),
            compute_key(&base_config(), exe.path()).unwrap()
        );
    }

    #[test]
    fn exe_mtime_changes_key() {
        let exe = exe();
        let before = compute_key(&base_config(), exe.path()).unwrap();
        let file = File::options().write(true).open(exe.path()).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(1))
            .unwrap();
        let after = compute_key(&base_config(), exe.path()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn qdrant_url_hashes_raw_bin() {
        let exe = exe();
        let mut config = base_config();
        config.qdrant_url = Some("http://x:1".into());
        config.qdrant_bin = "definitely-not-on-path".into();
        assert!(compute_key(&config, exe.path()).is_ok());
    }

    #[test]
    fn key_is_16_lowercase_hex() {
        let exe = exe();
        let key = compute_key(&base_config(), exe.path()).unwrap();
        assert_eq!(key.len(), 16);
        assert!(
            key.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn socket_path_too_long_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("n".repeat(60)).join("m".repeat(60));
        unsafe { std::env::set_var("FERREX_RUN_DIR", &deep) };
        let exe = exe();
        let err = DaemonPaths::for_config(&base_config(), exe.path()).unwrap_err();
        assert!(err.to_string().contains(deep.to_str().unwrap()), "{err}");
    }

    #[test]
    fn relative_run_dir_is_anchored_to_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("FERREX_RUN_DIR", "rel/run") };
        std::env::set_current_dir(tmp.path()).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(run_dir().unwrap(), cwd.join("rel/run"));
    }

    #[test]
    fn relative_run_dir_with_deleted_cwd_names_the_problem() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("gone");
        fs::create_dir(&gone).unwrap();
        std::env::set_current_dir(&gone).unwrap();
        fs::remove_dir(&gone).unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("FERREX_RUN_DIR", "rel") };
        let err = run_dir().unwrap_err();
        assert!(err.to_string().contains("current directory"), "{err}");
    }

    #[test]
    fn absolute_run_dir_is_used_as_is() {
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("FERREX_RUN_DIR", "/abs/run") };
        assert_eq!(run_dir().unwrap(), PathBuf::from("/abs/run"));
    }

    #[test]
    fn macos_tempdir_socket_fits() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join("x".repeat(64 - tmp.path().as_os_str().len() - 1));
        assert_eq!(dir.as_os_str().len(), 64);
        unsafe { std::env::set_var("FERREX_RUN_DIR", &dir) };
        let exe = exe();
        let paths = DaemonPaths::for_config(&base_config(), exe.path()).unwrap();
        assert!(paths.socket.as_os_str().len() <= MAX_SUN_PATH);
    }

    #[test]
    fn pid_file_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("k.pid");
        let pid = PidFile {
            pid: 4242,
            db_path: PathBuf::from("/abs/x.db"),
        };
        pid.write(&path).unwrap();
        assert_eq!(PidFile::read(&path), Some(pid));
        assert!(!tmp.path().join("k.pid.tmp").exists());
    }

    #[test]
    fn pid_file_unparsable_reads_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("k.pid");
        fs::write(&path, "not json").unwrap();
        assert_eq!(PidFile::read(&path), None);
        assert_eq!(PidFile::read(Path::new("/nonexistent/k.pid")), None);
    }

    fn mode(dir: &Path) -> u32 {
        fs::metadata(dir).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn ensure_run_dir_creates_missing_dirs_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/run");
        ensure_run_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&tmp.path().join("a")), 0o700);
    }

    #[test]
    fn ensure_run_dir_accepts_existing_private_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        ensure_run_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn ensure_run_dir_refuses_existing_open_dir_without_chmod() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("shared");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let err = ensure_run_dir(&dir).unwrap_err().to_string();
        assert!(err.contains(dir.to_str().unwrap()), "{err}");
        assert!(err.contains("chmod 700"), "{err}");
        assert_eq!(mode(&dir), 0o755);
    }

    #[test]
    fn ensure_run_dir_refuses_dir_owned_by_someone_else() {
        // Root owns /, so this runs only as a regular user.
        if rustix::process::geteuid().is_root() {
            return;
        }
        let err = ensure_run_dir(Path::new("/")).unwrap_err().to_string();
        assert!(err.contains("owned by"), "{err}");
    }

    fn live_daemon(dir: &Path, stem: &str, db: &str) -> File {
        PidFile {
            pid: std::process::id(),
            db_path: PathBuf::from(db),
        }
        .write_locked(&dir.join(format!("{stem}.pid")))
        .unwrap()
    }

    #[test]
    fn live_for_db_does_not_connect_to_the_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let listener =
            std::os::unix::net::UnixListener::bind(tmp.path().join("aaaa.sock")).unwrap();
        let _lock = live_daemon(tmp.path(), "aaaa", "/abs/x.db");
        assert!(live_for_db(tmp.path(), Path::new("/abs/x.db")).is_some());
        listener.set_nonblocking(true).unwrap();
        let err = listener.accept().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn live_for_db_ignores_unlocked_pid_file_with_listening_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let _listener =
            std::os::unix::net::UnixListener::bind(tmp.path().join("aaaa.sock")).unwrap();
        PidFile {
            pid: std::process::id(),
            db_path: PathBuf::from("/abs/x.db"),
        }
        .write(&tmp.path().join("aaaa.pid"))
        .unwrap();
        assert_eq!(live_for_db(tmp.path(), Path::new("/abs/x.db")), None);
    }

    #[test]
    fn pid_file_lock_survives_rewrite_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("aaaa.pid");
        let lock = live_daemon(tmp.path(), "aaaa", "/abs/x.db");
        assert!(pid_file_held(&path));
        drop(lock);
        assert!(!pid_file_held(&path));
        assert!(!pid_file_held(&tmp.path().join("missing.pid")));
    }

    #[test]
    fn live_for_db_finds_same_db_any_key() {
        let tmp = tempfile::tempdir().unwrap();
        let _l = live_daemon(tmp.path(), "aaaa", "/abs/x.db");
        assert_eq!(
            live_for_db(tmp.path(), Path::new("/abs/x.db")),
            Some(std::process::id())
        );
    }

    #[test]
    fn live_for_db_ignores_other_db() {
        let tmp = tempfile::tempdir().unwrap();
        let _l = live_daemon(tmp.path(), "aaaa", "/abs/x.db");
        assert_eq!(live_for_db(tmp.path(), Path::new("/abs/y.db")), None);
    }

    #[test]
    fn live_for_db_ignores_stale_pid_file() {
        let tmp = tempfile::tempdir().unwrap();
        PidFile {
            pid: std::process::id(),
            db_path: PathBuf::from("/abs/x.db"),
        }
        .write(&tmp.path().join("aaaa.pid"))
        .unwrap();
        assert_eq!(live_for_db(tmp.path(), Path::new("/abs/x.db")), None);
    }

    #[test]
    fn live_for_db_skips_unparsable_pid_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("x.pid"), "garbage").unwrap();
        let _l = live_daemon(tmp.path(), "bbbb", "/abs/x.db");
        assert_eq!(
            live_for_db(tmp.path(), Path::new("/abs/x.db")),
            Some(std::process::id())
        );
        assert_eq!(live_for_db(tmp.path(), Path::new("/abs/none.db")), None);
    }

    #[test]
    fn live_for_db_ignores_dead_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let _l = std::os::unix::net::UnixListener::bind(tmp.path().join("aaaa.sock")).unwrap();
        PidFile {
            pid: dead,
            db_path: PathBuf::from("/abs/x.db"),
        }
        .write(&tmp.path().join("aaaa.pid"))
        .unwrap();
        assert_eq!(live_for_db(tmp.path(), Path::new("/abs/x.db")), None);
    }

    #[test]
    fn any_live_finds_daemon_on_other_db() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(any_live(tmp.path()), None);
        let _l = live_daemon(tmp.path(), "aaaa", "/abs/other.db");
        assert_eq!(any_live(tmp.path()), Some(std::process::id()));
    }
}
