mod audit;
mod backfill;

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
}

#[derive(Subcommand)]
enum AuditCommand {
    Reconcile {
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        sample: Option<usize>,
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

fn resolve_config_path(cli: &Cli) -> PathBuf {
    cli.config_path.clone().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".ferrex")
            .join("ferrex.toml")
    })
}

fn build_config(cli: Cli) -> eyre::Result<FerrexConfig> {
    let db_path = cli.db_path.clone().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".ferrex")
            .join("ferrex.db")
    });
    let config_path = resolve_config_path(&cli);
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
    })
}

#[derive(Deserialize, JsonSchema)]
struct StoreParams {
    /// The memory content. Required for episodic and procedural memories.
    content: Option<String>,
    /// Memory type: "episodic", "semantic", or "procedural". Auto-detected if omitted.
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
struct RecallParams {
    /// Search query — what are you looking for?
    query: String,
    /// Filter by memory types: `["episodic"]`, `["semantic"]`, etc.
    types: Option<Vec<String>>,
    /// Filter by entity names. Returns memories mentioning any of these entities.
    entities: Option<Vec<String>>,
    /// Namespace override.
    namespace: Option<String>,
    /// Max results (default 10).
    limit: Option<usize>,
    /// Memory IDs to mark as validated (confirmed still accurate).
    validate_ids: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
struct ForgetParams {
    /// Memory IDs to forget.
    ids: Vec<String>,
    /// Cascade delete linked entities (deprecated, ignored).
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
    /// Return detailed stats including per-type breakdown and staleness distribution.
    detailed: Option<bool>,
}

#[derive(Clone)]
struct FerrexServer {
    service: Arc<MemoryService>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl FerrexServer {
    fn new(service: MemoryService) -> Self {
        Self {
            service: Arc::new(service),
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
        description = "Save a memory. Episodic: events and observations (provide content). Semantic: facts as subject-predicate-object triples. Procedural: workflows (provide content, set type to 'procedural'). Type auto-detects when omitted."
    )]
    async fn store(&self, Parameters(p): Parameters<StoreParams>) -> Result<String, ErrorData> {
        let memory_type = p
            .memory_type
            .as_deref()
            .map(str::parse::<ferrex_core::MemoryType>)
            .transpose()
            .map_err(|e| ErrorData::invalid_params(e, None))?;

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
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "stored": true,
            "id": resp.id,
            "type": resp.memory_type,
            "superseded": resp.superseded,
        }))
        .unwrap_or_default())
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

        let req = RecallRequest {
            query: p.query,
            types,
            entities: p.entities,
            namespace: p.namespace,
            limit: p.limit,
            include_stale: None,
            include_invalidated: None,
            time_range: None,
            validate_ids: p.validate_ids,
        };

        let results = self.service.recall(req).await.map_err(map_error)?;
        let output: Vec<serde_json::Value> = results
            .into_iter()
            .map(|r| {
                serde_json::json!({
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
                })
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

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match cli.command.take() {
        Some(Command::Audit {
            audit: AuditCommand::Reconcile { fix, sample },
        }) => {
            return audit::run_reconcile(build_config(cli)?, fix, sample);
        }
        Some(Command::Backfill {
            backfill: BackfillCommand::NormalizedPredicates { namespace, dry_run },
        }) => {
            return backfill::run_normalized_predicates(build_config(cli)?, namespace, dry_run);
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
            let server = FerrexServer::new(service);
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

            if let Some(ref mut sc) = sidecar {
                sc.shutdown();
            }
            Ok(())
        })
}
