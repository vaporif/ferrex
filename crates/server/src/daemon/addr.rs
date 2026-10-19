use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ferrex_core::FerrexConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(target_os = "macos")]
pub const MAX_SUN_PATH: usize = 103;
#[cfg(not(target_os = "macos"))]
pub const MAX_SUN_PATH: usize = 107;

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

#[must_use]
pub fn run_dir() -> PathBuf {
    std::env::var_os("FERREX_RUN_DIR").map_or_else(
        || {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".ferrex")
                .join("run")
        },
        PathBuf::from,
    )
}

pub fn ensure_run_dir(dir: &Path) -> eyre::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| eyre::eyre!("create run dir {}: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| eyre::eyre!("chmod run dir {}: {e}", dir.display()))
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
        let run_dir = run_dir();
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
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)
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
                |e| tracing::debug!(path = %path.display(), error = %e, "pid file unparseable"),
            )
            .ok()
    }
}

#[cfg(test)]
mod tests {
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

    fn base_config() -> FerrexConfig {
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
    fn pid_file_unparseable_reads_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("k.pid");
        fs::write(&path, "not json").unwrap();
        assert_eq!(PidFile::read(&path), None);
        assert_eq!(PidFile::read(Path::new("/nonexistent/k.pid")), None);
    }

    #[test]
    fn ensure_run_dir_sets_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_run_dir(&dir).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
