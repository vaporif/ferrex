use std::path::{Path, PathBuf};

use clap::{ArgAction, Parser, Subcommand};
use ferrex_core::{FerrexConfig, ModelTier, RerankerTier};

#[derive(Parser)]
#[command(name = "ferrex", about = "Local-first MCP memory server for AI agents")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(long, env = "FERREX_QDRANT_URL")]
    pub qdrant_url: Option<String>,

    #[arg(long, env = "FERREX_QDRANT_BIN", default_value = "qdrant")]
    pub qdrant_bin: String,

    #[arg(long, env = "FERREX_QDRANT_PORT", default_value_t = 6334)]
    pub qdrant_port: u16,

    #[arg(long, env = "FERREX_MODEL_TIER", default_value = "best")]
    pub model_tier: ModelTier,

    #[arg(long, env = "FERREX_RERANKER_TIER", default_value = "default")]
    pub reranker_tier: RerankerTier,

    #[arg(long, env = "FERREX_NAMESPACE", default_value = "default")]
    pub namespace: String,

    #[arg(long, env = "FERREX_DB_PATH")]
    pub db_path: Option<PathBuf>,

    #[arg(long, env = "FERREX_CONFIG_PATH")]
    pub config_path: Option<PathBuf>,

    /// Serve MCP in this process instead of through the shared daemon.
    #[arg(
        long,
        env = "FERREX_NO_DAEMON",
        action = ArgAction::SetTrue,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub no_daemon: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Check Qdrant/SQLite consistency.
    Audit {
        #[command(subcommand)]
        audit: AuditCommand,
    },
    /// Backfill missing normalized predicates.
    Backfill {
        #[command(subcommand)]
        backfill: BackfillCommand,
    },
    Diagnose,
    Journal {
        #[command(subcommand)]
        journal: JournalCommand,
    },
    /// Delete all data (SQLite DB, Qdrant storage, PID/lock files).
    Nuke {
        /// Skip confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// Re-embed all memories with current model tier.
    ReEmbed {
        #[arg(long)]
        dry_run: bool,
    },
    Backup {
        #[arg(long)]
        output: Option<PathBuf>,
    },
    Restore {
        #[arg(long)]
        from: PathBuf,
    },
    /// Print the shared-daemon key and run-dir paths as JSON.
    #[command(hide = true)]
    DaemonPaths,
    /// Run the shared daemon for `key`; spawned by the proxy.
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        key: String,
    },
}

#[derive(Subcommand)]
pub enum AuditCommand {
    Reconcile {
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        sample: Option<usize>,
        #[arg(long, default_value = "text")]
        format: String,
    },
}

#[derive(Subcommand)]
pub enum JournalCommand {
    Show {
        /// "pending", "completed", or "failed".
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// "1h", "24h", or "7d".
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value = "text")]
        format: String,
    },
}

#[derive(Subcommand)]
pub enum BackfillCommand {
    NormalizedPredicates {
        #[arg(long)]
        namespace: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

const SHIPPED_BASELINE: &str = include_str!("../config/ferrex.toml");

fn absolutize(path: &Path) -> eyre::Result<PathBuf> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::env::current_dir()?,
    };
    std::fs::create_dir_all(&parent)
        .map_err(|e| eyre::eyre!("create {}: {e}", parent.display()))?;
    let parent = parent
        .canonicalize()
        .map_err(|e| eyre::eyre!("resolve {}: {e}", parent.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| eyre::eyre!("path has no file name: {}", path.display()))?;
    Ok(parent.join(name))
}

/// An in-memory database can't be shared, so it always runs standalone.
#[must_use]
pub fn use_standalone(no_daemon: bool, config: &FerrexConfig) -> bool {
    no_daemon || ferrex_store::is_memory_path(&config.db_path)
}

pub fn build_config(cli: Cli) -> eyre::Result<FerrexConfig> {
    let default_dir = || {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".ferrex")
    };
    let db_path = cli
        .db_path
        .clone()
        .unwrap_or_else(|| default_dir().join("ferrex.db"));
    let config_path = cli
        .config_path
        .clone()
        .unwrap_or_else(|| default_dir().join("ferrex.toml"));
    let db_path = if ferrex_store::is_memory_path(&db_path) {
        db_path
    } else {
        absolutize(&db_path)?
    };
    let loaded = ferrex_core::load_or_init(&config_path, SHIPPED_BASELINE)
        .map_err(|e| eyre::eyre!("config load: {e}"))?;
    let config_path = absolutize(&config_path)?;
    Ok(FerrexConfig {
        qdrant_url: cli.qdrant_url,
        qdrant_bin: cli.qdrant_bin,
        qdrant_port: cli.qdrant_port,
        model_tier: cli.model_tier,
        reranker_tier: cli.reranker_tier,
        namespace: cli.namespace,
        db_path,
        config_path: Some(config_path),
        deduplication: loaded.deduplication,
        conflict: loaded.conflict,
        predicates: loaded.predicates,
        reconciliation: loaded.reconciliation,
        staleness: loaded.staleness,
        reader_pool_size: loaded.reader_pool_size,
        cache: loaded.cache,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_for(args: &[&str]) -> eyre::Result<FerrexConfig> {
        let mut argv = vec!["ferrex"];
        argv.extend_from_slice(args);
        build_config(Cli::try_parse_from(argv)?)
    }

    fn standalone_for(args: &[&str]) -> bool {
        let mut argv = vec!["ferrex"];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv).unwrap();
        let no_daemon = cli.no_daemon;
        use_standalone(no_daemon, &build_config(cli).unwrap())
    }

    #[test]
    fn no_daemon_env_one_selects_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("FERREX_NO_DAEMON", "1") };
        assert!(standalone_for(&[
            "--config-path",
            "c.toml",
            "--db-path",
            "x.db"
        ]));
    }

    #[test]
    fn no_daemon_env_zero_uses_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("FERREX_NO_DAEMON", "0") };
        assert!(!standalone_for(&[
            "--config-path",
            "c.toml",
            "--db-path",
            "x.db"
        ]));
    }

    #[test]
    fn no_daemon_flag_selects_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::remove_var("FERREX_NO_DAEMON") };
        assert!(!standalone_for(&[
            "--config-path",
            "c.toml",
            "--db-path",
            "x.db"
        ]));
        assert!(standalone_for(&[
            "--no-daemon",
            "--config-path",
            "c.toml",
            "--db-path",
            "x.db"
        ]));
    }

    #[test]
    fn memory_db_path_selects_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::remove_var("FERREX_NO_DAEMON") };
        assert!(standalone_for(&[
            "--config-path",
            "c.toml",
            "--db-path",
            ":memory:"
        ]));
    }

    #[test]
    fn build_config_absolutizes_relative_db_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let config = config_for(&["--config-path", "c.toml", "--db-path", "sub/x.db"]).unwrap();
        let expected = tmp.path().canonicalize().unwrap().join("sub/x.db");
        assert_eq!(config.db_path, expected);
        assert!(tmp.path().join("sub").is_dir());
    }

    #[test]
    fn build_config_same_key_path_for_different_spellings() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("c.toml");
        let cfg = cfg.to_str().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        let dotted = config_for(&["--config-path", cfg, "--db-path", "./a/../b/x.db"]).unwrap();
        let abs = tmp.path().join("b/x.db");
        let plain =
            config_for(&["--config-path", cfg, "--db-path", abs.to_str().unwrap()]).unwrap();
        assert_eq!(dotted.db_path, plain.db_path);
    }

    #[test]
    fn build_config_keeps_memory_db_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let config = config_for(&["--config-path", "c.toml", "--db-path", ":memory:"]).unwrap();
        assert_eq!(config.db_path, Path::new(":memory:"));
        assert!(!tmp.path().join(":memory:").exists());
    }

    #[test]
    fn build_config_absolutizes_config_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let config =
            config_for(&["--config-path", "c/ferrex.toml", "--db-path", ":memory:"]).unwrap();
        let path = config.config_path.unwrap();
        assert!(path.is_absolute());
        assert!(path.is_file());
    }

    #[test]
    fn invalid_namespace_is_rejected_at_parse_time() {
        for bad in ["bad/name", "", "a b"] {
            let err = Cli::try_parse_from(["ferrex", "--namespace", bad])
                .err()
                .unwrap_or_else(|| panic!("{bad:?} parsed"));
            assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation, "{err}");
        }
        for good in ["default", "proj_1-x"] {
            let cli = Cli::try_parse_from(["ferrex", "--namespace", good]).unwrap();
            assert_eq!(cli.namespace, good);
        }
    }
}
