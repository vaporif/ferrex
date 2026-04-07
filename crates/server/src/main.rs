mod audit;
mod backfill;
mod hint;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use ferrex_core::{
    CoreError, FerrexConfig, ForgetRequest, MemoryService, ModelTier, RecallRequest,
    ReflectRequest, RerankerTier, StatsRequest, StoreRequest,
};
use rmcp::{
    ErrorData, ServerHandler, ServiceExt, handler::server::wrapper::Parameters, tool, tool_handler,
    tool_router, transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Parser)]
#[command(name = "ferrex", about = "Local-first MCP memory server for AI agents")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(long, env = "FERREX_QDRANT_URL")]
    qdrant_url: Option<String>,

    #[arg(long, env = "FERREX_QDRANT_BIN", default_value = "qdrant")]
    qdrant_bin: String,

    #[arg(long, env = "FERREX_QDRANT_PORT", default_value_t = 6334)]
    qdrant_port: u16,

    #[arg(long, env = "FERREX_MODEL_TIER", default_value = "best")]
    model_tier: ModelTier,

    #[arg(long, env = "FERREX_RERANKER_TIER", default_value = "default")]
    reranker_tier: RerankerTier,

    #[arg(long, env = "FERREX_NAMESPACE", default_value = "default")]
    namespace: String,

    #[arg(long, env = "FERREX_DB_PATH")]
    db_path: Option<PathBuf>,

    #[arg(long, env = "FERREX_CONFIG_PATH")]
    config_path: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Audit consistency between Qdrant and `SQLite`.
    Audit {
        #[command(subcommand)]
        audit: AuditCommand,
    },
    /// Backfill `normalized_predicate` for semantic memories missing it.
    Backfill {
        #[command(subcommand)]
        backfill: BackfillCommand,
    },
    /// Print system diagnostics.
    Diagnose,
    /// Inspect the transaction journal.
    Journal {
        #[command(subcommand)]
        journal: JournalCommand,
    },
    /// Re-embed all memories with the current model tier.
    ReEmbed {
        #[arg(long)]
        dry_run: bool,
    },
    /// Create a backup of the ferrex database.
    Backup {
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Restore a ferrex database from a backup.
    Restore {
        #[arg(long)]
        from: PathBuf,
    },
}

#[derive(Subcommand)]
enum AuditCommand {
    Reconcile {
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        sample: Option<usize>,
        /// Output format: "text" or "json".
        #[arg(long, default_value = "text")]
        format: String,
    },
}

#[derive(Subcommand)]
enum JournalCommand {
    /// Show journal entries.
    Show {
        /// Filter: "pending", "completed", "failed".
        #[arg(long)]
        status: Option<String>,
        /// Max entries to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Time filter: "1h", "24h", "7d".
        #[arg(long)]
        since: Option<String>,
        /// Output format: "text" or "json".
        #[arg(long, default_value = "text")]
        format: String,
    },
}

#[derive(Subcommand)]
enum BackfillCommand {
    NormalizedPredicates {
        #[arg(long)]
        namespace: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

const SHIPPED_BASELINE: &str = include_str!("../config/ferrex.toml");

fn build_config(cli: Cli) -> eyre::Result<FerrexConfig> {
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

#[derive(Deserialize, JsonSchema)]
struct StoreParams {
    /// Required for episodic and procedural memories.
    content: Option<String>,
    /// "episodic", "semantic", or "procedural". Detected if omitted.
    memory_type: Option<String>,
    /// Subject of a semantic triple (e.g. "api-server").
    subject: Option<String>,
    /// Predicate of a semantic triple (e.g. "uses").
    predicate: Option<String>,
    /// Object of a semantic triple (e.g. "tokio 1.38").
    object: Option<String>,
    /// Confidence score 0.0-1.0. Default 1.0.
    confidence: Option<f64>,
    /// Where this memory came from.
    source: Option<String>,
    /// Additional context as JSON.
    context: Option<serde_json::Value>,
    /// Entity names mentioned in this memory.
    #[serde(default)]
    entities: Vec<String>,
    /// Namespace override.
    namespace: Option<String>,
    /// ID of a memory this supersedes.
    supersedes: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct TimeRangeParam {
    /// Start of time range (ISO-8601, e.g. "2025-01-01T00:00:00Z"). Inclusive.
    start: Option<String>,
    /// End of time range (ISO-8601, e.g. "2025-12-31T23:59:59Z"). Inclusive.
    end: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RecallParams {
    /// Search query.
    query: String,
    /// Filter by memory types, e.g. `["episodic"]`, `["semantic"]`.
    types: Option<Vec<String>>,
    /// Filter by entity names.
    entities: Option<Vec<String>>,
    /// Namespace override.
    namespace: Option<String>,
    /// Max results (default 10).
    limit: Option<usize>,
    /// Memory IDs to mark as still accurate.
    validate_ids: Option<Vec<String>>,
    /// Only return memories created within this time range.
    time_range: Option<TimeRangeParam>,
    /// Include superseded memories. Default: false.
    include_invalidated: Option<bool>,
    /// Set false to exclude stale memories.
    include_stale: Option<bool>,
    /// Include scoring breakdown for each result. Default: false.
    #[serde(default)]
    explain: bool,
}

#[derive(Deserialize, JsonSchema)]
struct ForgetParams {
    /// Memory IDs to forget.
    ids: Vec<String>,
    /// Deprecated, ignored.
    cascade: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ReflectParams {
    /// Memory namespace to audit.
    namespace: String,
    /// Max candidates to return (default 20).
    limit: Option<u32>,
    /// Include contradiction pairs (default true).
    include_contradictions: Option<bool>,
    /// Include stale memory candidates (default true).
    include_stale: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct StatsParams {
    /// Memory namespace.
    namespace: String,
    /// Include per-type breakdown and staleness distribution.
    detailed: Option<bool>,
    /// Include system diagnostics (`Qdrant` status, `SQLite` size, cache stats, recent ops).
    diagnostics: Option<bool>,
}

#[derive(Clone)]
struct FerrexServer {
    service: Arc<MemoryService>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl FerrexServer {
    fn new(service: Arc<MemoryService>) -> Self {
        Self {
            service,
            tool_router: Self::tool_router(),
        }
    }
}

fn map_error(e: CoreError) -> ErrorData {
    match e {
        CoreError::Validation(msg) => ErrorData::invalid_params(msg, None),
        CoreError::Duplicate {
            existing_id,
            similarity,
        } => ErrorData::invalid_params(
            format!("duplicate: existing_id={existing_id} similarity={similarity:.4}"),
            Some(serde_json::json!({
                "code": "duplicate",
                "existing_id": existing_id,
                "similarity": similarity,
            })),
        ),
        CoreError::ConflictAmbiguous { existing_id, ratio } => ErrorData::invalid_params(
            format!("conflict_ambiguous: existing_id={existing_id} ratio={ratio:.4}"),
            Some(serde_json::json!({
                "code": "conflict_ambiguous",
                "existing_id": existing_id,
                "ratio": ratio,
            })),
        ),
        CoreError::MultiMatchConflict { existing_ids } => ErrorData::invalid_params(
            format!("multi_match_conflict: existing_ids={existing_ids:?}"),
            Some(serde_json::json!({
                "code": "multi_match_conflict",
                "existing_ids": existing_ids,
            })),
        ),
        other => ErrorData::internal_error(other.to_string(), None),
    }
}

#[tool_router]
impl FerrexServer {
    #[tool(
        name = "store",
        description = "Save a memory. Auto-detects type: subject+predicate+object = semantic, otherwise = episodic. For workflows and runbooks, set memory_type='procedural' -- they persist 12x longer."
    )]
    async fn store(&self, Parameters(p): Parameters<StoreParams>) -> Result<String, ErrorData> {
        let memory_type = p
            .memory_type
            .as_deref()
            .map(str::parse::<ferrex_core::MemoryType>)
            .transpose()
            .map_err(|e| ErrorData::invalid_params(e, None))?;

        let content_for_hint = p.content.clone();

        let req = StoreRequest {
            content: p.content,
            memory_type,
            subject: p.subject,
            predicate: p.predicate,
            object: p.object,
            confidence: p.confidence,
            source: p.source,
            context: p.context,
            entities: p.entities,
            namespace: p.namespace,
            supersedes: p.supersedes,
        };

        let resp = self.service.store(req).await.map_err(map_error)?;

        let mut json = serde_json::json!({
            "stored": true,
            "id": resp.id,
            "type": resp.memory_type,
            "superseded": resp.superseded,
        });

        if resp.memory_type == "episodic"
            && let Some(ref content) = content_for_hint
            && hint::looks_like_workflow(content)
        {
            json["hint"] = serde_json::json!(
                "This looks like a workflow. Procedural memories persist 12x longer \
                 (365d vs 30d half-life). Re-store with memory_type: 'procedural' \
                 if this should be long-lived."
            );
        }

        Ok(serde_json::to_string_pretty(&json).unwrap_or_default())
    }

    #[tool(
        name = "recall",
        description = "Search memories by semantic similarity. Returns the most relevant memories matching your query. Filter by type or entity names. Use this when you need to remember something."
    )]
    async fn recall(&self, Parameters(p): Parameters<RecallParams>) -> Result<String, ErrorData> {
        let types = p
            .types
            .map(|ts| {
                ts.iter()
                    .map(|s| s.parse::<ferrex_core::MemoryType>())
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(|e| ErrorData::invalid_params(e, None))?;

        let time_range = p
            .time_range
            .map(|tr| {
                let parse = |s: &str| -> Result<chrono::DateTime<chrono::Utc>, ErrorData> {
                    s.parse::<chrono::DateTime<chrono::Utc>>().map_err(|e| {
                        ErrorData::invalid_params(format!("invalid datetime: {e}"), None)
                    })
                };
                let range = ferrex_core::TimeRange {
                    start: tr.start.as_deref().map(parse).transpose()?,
                    end: tr.end.as_deref().map(parse).transpose()?,
                };
                if let (Some(s), Some(e)) = (range.start, range.end)
                    && s > e
                {
                    return Err(ErrorData::invalid_params(
                        "time_range start must be <= end",
                        None,
                    ));
                }
                Ok(range)
            })
            .transpose()?;

        let req = RecallRequest {
            query: p.query,
            types,
            entities: p.entities,
            namespace: p.namespace,
            limit: p.limit,
            include_stale: p.include_stale,
            include_invalidated: p.include_invalidated,
            time_range,
            validate_ids: p.validate_ids,
            explain: p.explain,
        };

        let results = self.service.recall(req).await.map_err(map_error)?;
        let output: Vec<serde_json::Value> = results
            .into_iter()
            .map(|r| {
                let mut obj = serde_json::json!({
                    "id": r.memory.id,
                    "type": r.memory.memory_type,
                    "content": r.memory.content,
                    "subject": r.memory.subject,
                    "predicate": r.memory.predicate,
                    "object": r.memory.object,
                    "score": r.relevance_score,
                    "staleness_score": r.staleness_score,
                    "freshness": r.freshness_label,
                    "entities": r.memory.entities,
                    "created_at": r.memory.created_at.to_rfc3339(),
                });
                if let Some(ref scoring) = r.scoring
                    && let Some(map) = obj.as_object_mut()
                    && let Ok(val) = serde_json::to_value(scoring)
                {
                    map.insert("scoring".to_string(), val);
                }
                obj
            })
            .collect();
        Ok(serde_json::to_string_pretty(&output).unwrap_or_default())
    }

    #[tool(
        name = "forget",
        description = "Delete memories by ID. You must recall first to find the IDs you want to forget."
    )]
    async fn forget(&self, Parameters(p): Parameters<ForgetParams>) -> Result<String, ErrorData> {
        let req = ForgetRequest {
            ids: p.ids,
            cascade: p.cascade,
        };
        let resp = self.service.forget(req).await.map_err(map_error)?;
        Ok(serde_json::to_string_pretty(&resp).unwrap_or_default())
    }

    #[tool(
        name = "reflect",
        description = "Audit memory health. Surfaces stale memories, contradictions, and low-access candidates for cleanup."
    )]
    async fn reflect(&self, Parameters(p): Parameters<ReflectParams>) -> Result<String, ErrorData> {
        let req = ReflectRequest {
            namespace: p.namespace,
            limit: p.limit,
            include_contradictions: p.include_contradictions.unwrap_or(true),
            include_stale: p.include_stale.unwrap_or(true),
        };
        let resp = self.service.reflect(req).await.map_err(map_error)?;
        Ok(serde_json::to_string_pretty(&resp).unwrap_or_default())
    }

    #[tool(
        name = "stats",
        description = "Overview of the memory system. Shows total count, recent memories, and items needing attention. Call this at conversation start to orient yourself."
    )]
    async fn stats(&self, Parameters(p): Parameters<StatsParams>) -> Result<String, ErrorData> {
        let req = StatsRequest {
            namespace: p.namespace,
            detailed: p.detailed,
            diagnostics: p.diagnostics,
        };
        let resp = self.service.stats(req).await.map_err(map_error)?;
        Ok(serde_json::to_string_pretty(&resp).unwrap_or_default())
    }
}

#[tool_handler]
impl ServerHandler for FerrexServer {}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let mut cli = Cli::parse();

    let env_filter = tracing_subscriber::EnvFilter::try_from_env("FERREX_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let log_file = std::env::var("FERREX_LOG_FILE").ok().map(|p| {
        let path = PathBuf::from(p);
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let name = path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("ferrex.log"));
        tracing_appender::rolling::never(dir, name)
    });

    let log_format = std::env::var("FERREX_LOG_FORMAT").unwrap_or_default();
    let is_json = log_format.eq_ignore_ascii_case("json");

    match (log_file, is_json) {
        (Some(file), true) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(file)
                .json()
                .init();
        }
        (Some(file), false) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(file)
                .with_ansi(false)
                .init();
        }
        (None, true) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .json()
                .init();
        }
        (None, false) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .init();
        }
    }

    match cli.command.take() {
        Some(Command::Audit {
            audit:
                AuditCommand::Reconcile {
                    fix,
                    sample,
                    format,
                },
        }) => {
            return audit::run_reconcile(build_config(cli)?, fix, sample, &format);
        }
        Some(Command::Backfill {
            backfill: BackfillCommand::NormalizedPredicates { namespace, dry_run },
        }) => {
            return backfill::run_normalized_predicates(build_config(cli)?, namespace, dry_run);
        }
        #[allow(clippy::cast_precision_loss)]
        Some(Command::Diagnose) => {
            let config = build_config(cli)?;
            return tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(async move {
                    let service = MemoryService::from_config(config).await?;
                    let report = service.diagnostics().await?;

                    println!("ferrex v{} | {}", report.version, report.embedding_model);
                    println!();
                    println!("Qdrant:");
                    println!(
                        "  url: {}{}",
                        report.qdrant_url,
                        report
                            .qdrant_pid
                            .map(|p| format!(" (sidecar, pid {p})"))
                            .unwrap_or_default()
                    );
                    println!(
                        "  collection: {} -- {} points",
                        report.collection, report.memory_count
                    );
                    println!();
                    println!("SQLite:");
                    println!(
                        "  path: {} -- {:.1} MB",
                        report.sqlite_path,
                        report.sqlite_size_bytes as f64 / 1_048_576.0
                    );
                    println!(
                        "  memories: {} | entities: {}",
                        report.memory_count, report.entity_count
                    );
                    println!("  pending ops: {}", report.pending_ops);
                    println!();
                    println!("Cache:");
                    let emb_total = report.cache.embedding_hits + report.cache.embedding_misses;
                    let emb_rate = if emb_total > 0 {
                        report.cache.embedding_hits as f64 / emb_total as f64 * 100.0
                    } else {
                        0.0
                    };
                    let res_total = report.cache.result_hits + report.cache.result_misses;
                    let res_rate = if res_total > 0 {
                        report.cache.result_hits as f64 / res_total as f64 * 100.0
                    } else {
                        0.0
                    };
                    println!(
                        "  embedding: {}/{} entries, {:.0}% hit rate",
                        report.cache.embedding_len, report.cache.embedding_capacity, emb_rate
                    );
                    println!(
                        "  result: {}/{} entries, {:.0}% hit rate",
                        report.cache.result_len, report.cache.result_capacity, res_rate
                    );

                    if !report.recent_ops.is_empty() {
                        println!();
                        println!("Last {} operations:", report.recent_ops.len());
                        for op in &report.recent_ops {
                            let ago = chrono::Utc::now() - op.timestamp;
                            let ago_str = if ago.num_seconds() < 60 {
                                format!("{}s ago", ago.num_seconds())
                            } else if ago.num_minutes() < 60 {
                                format!("{}m ago", ago.num_minutes())
                            } else {
                                format!("{}h ago", ago.num_hours())
                            };
                            println!(
                                "  {:<8} {:<30} {:>6}ms  {:<12} {}",
                                op.kind,
                                op.detail.chars().take(30).collect::<String>(),
                                op.duration_ms,
                                op.outcome,
                                ago_str
                            );
                        }
                    }

                    service.shutdown().await;
                    Ok::<_, eyre::Report>(())
                });
        }
        Some(Command::Journal {
            journal:
                JournalCommand::Show {
                    status,
                    limit,
                    since,
                    format,
                },
        }) => {
            let config = build_config(cli)?;
            return tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(async move {
                    let service = MemoryService::from_config(config).await?;

                    let since_dt = since
                        .as_deref()
                        .map(|s| -> eyre::Result<_> {
                            let now = chrono::Utc::now();
                            match s {
                                "1h" => Ok(now - chrono::Duration::hours(1)),
                                "24h" => Ok(now - chrono::Duration::hours(24)),
                                "7d" => Ok(now - chrono::Duration::days(7)),
                                _ => Err(eyre::eyre!(
                                    "invalid --since value: {s}. Use 1h, 24h, or 7d."
                                )),
                            }
                        })
                        .transpose()?;

                    let status_filter = status.as_deref();
                    let show_pending = status_filter.is_none() || status_filter == Some("pending");
                    let show_completed = status_filter.is_none()
                        || status_filter == Some("completed")
                        || status_filter == Some("failed");

                    service.prune_journal().await?;

                    if format.eq_ignore_ascii_case("json") {
                        let mut json_obj = serde_json::Map::new();

                        if show_pending {
                            let pending = service.list_pending_ops().await?;
                            json_obj.insert(
                                "pending".to_string(),
                                serde_json::json!(
                                    pending
                                        .iter()
                                        .map(|op| serde_json::json!({
                                            "op_id": op.op_id,
                                            "kind": op.kind.as_str(),
                                            "memory_id": op.memory_id,
                                            "namespace": op.namespace,
                                            "started_at": op.started_at.to_rfc3339(),
                                        }))
                                        .collect::<Vec<_>>()
                                ),
                            );
                        }

                        if show_completed {
                            let completed = service
                                .list_completed_ops(status_filter, limit, since_dt)
                                .await?;
                            json_obj.insert(
                                "completed".to_string(),
                                serde_json::json!(
                                    completed
                                        .iter()
                                        .map(|op| serde_json::json!({
                                            "op_id": op.op_id,
                                            "kind": op.kind.as_str(),
                                            "memory_id": op.memory_id,
                                            "outcome": op.outcome,
                                            "duration_ms": op.duration_ms,
                                            "completed_at": op.completed_at.to_rfc3339(),
                                        }))
                                        .collect::<Vec<_>>()
                                ),
                            );
                        }

                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json_obj).unwrap_or_default()
                        );
                    } else {
                        if show_pending {
                            let pending = service.list_pending_ops().await?;
                            println!("Pending ({}):", pending.len());
                            if pending.is_empty() {
                                println!("  (none)");
                            } else {
                                for op in &pending {
                                    println!(
                                        "  {}  {}  {}  since {}",
                                        op.kind.as_str(),
                                        op.memory_id,
                                        op.namespace,
                                        op.started_at.format("%Y-%m-%d %H:%M:%S")
                                    );
                                }
                            }
                            println!();
                        }

                        if show_completed {
                            let completed = service
                                .list_completed_ops(status_filter, limit, since_dt)
                                .await?;
                            println!("Completed (last {}):", completed.len());
                            for op in &completed {
                                println!(
                                    "  {}  {}  {}  {:>5}  {:>6}ms",
                                    op.completed_at.format("%Y-%m-%d %H:%M:%S"),
                                    op.kind.as_str(),
                                    op.memory_id,
                                    op.outcome,
                                    op.duration_ms,
                                );
                            }
                        }
                    }

                    service.shutdown().await;
                    Ok::<_, eyre::Report>(())
                });
        }
        Some(Command::ReEmbed { dry_run: _ }) => {
            eprintln!("ferrex re-embed is not yet implemented");
            std::process::exit(1);
        }
        Some(Command::Backup { output: _ }) => {
            eprintln!("ferrex backup is not yet implemented");
            std::process::exit(1);
        }
        Some(Command::Restore { from: _ }) => {
            eprintln!("ferrex restore is not yet implemented");
            std::process::exit(1);
        }
        None => {}
    }

    let config = build_config(cli)?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let service = MemoryService::from_config(config).await?;
            let (service, mut sidecar) = service.into_parts();
            eprintln!("ferrex ready");
            let service = Arc::new(service);
            let server = FerrexServer::new(Arc::clone(&service));
            let (stdin, stdout) = stdio();
            let running = server
                .serve((stdin, stdout))
                .await
                .map_err(|e| eyre::eyre!("MCP server error: {e}"))?;

            #[cfg(unix)]
            {
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("failed to install SIGTERM handler");

                tokio::select! {
                    result = running.waiting() => {
                        result.map_err(|e| eyre::eyre!("MCP server error: {e}"))?;
                    }
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("received SIGINT, shutting down");
                    }
                    _ = sigterm.recv() => {
                        tracing::info!("received SIGTERM, shutting down");
                    }
                }
            }

            #[cfg(not(unix))]
            {
                tokio::select! {
                    result = running.waiting() => {
                        result.map_err(|e| eyre::eyre!("MCP server error: {e}"))?;
                    }
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("received SIGINT, shutting down");
                    }
                }
            }

            service.shutdown().await;
            if let Some(ref mut sc) = sidecar {
                sc.shutdown();
            }
            Ok(())
        })
}
