use std::path::PathBuf;

use clap::{Parser, Subcommand};
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
    let loaded = ferrex_core::load_or_init(&config_path, SHIPPED_BASELINE)
        .map_err(|e| eyre::eyre!("config load: {e}"))?;
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
