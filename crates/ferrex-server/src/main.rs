use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use ferrex_core::{
    CoreError, FerrexConfig, ForgetRequest, MemoryService, ModelTier, RecallRequest,
    ReflectRequest, StatsRequest, StoreRequest, TimeRange,
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
    #[arg(long, env = "FERREX_QDRANT_URL")]
    qdrant_url: Option<String>,

    #[arg(long, env = "FERREX_QDRANT_BIN", default_value = "qdrant")]
    qdrant_bin: String,

    #[arg(long, env = "FERREX_QDRANT_PORT", default_value_t = 6334)]
    qdrant_port: u16,

    #[arg(long, env = "FERREX_MODEL_TIER", default_value = "best")]
    model_tier: ModelTier,

    #[arg(long, env = "FERREX_NAMESPACE", default_value = "default")]
    namespace: String,

    #[arg(long, env = "FERREX_DB_PATH")]
    db_path: Option<PathBuf>,
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
    /// ID of a memory this supersedes. (Phase 3)
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
    /// Include stale memories (Phase 4).
    include_stale: Option<bool>,
    /// Include invalidated memories (Phase 4).
    include_invalidated: Option<bool>,
    /// Time range filter (Phase 2).
    time_range: Option<McpTimeRange>,
}

#[derive(Deserialize, JsonSchema)]
struct McpTimeRange {
    /// Start of time range (RFC 3339).
    start: Option<chrono::DateTime<chrono::Utc>>,
    /// End of time range (RFC 3339).
    end: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize, JsonSchema)]
struct ForgetParams {
    /// Memory IDs to forget.
    ids: Vec<String>,
    /// Cascade delete linked entities (Phase 3).
    cascade: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ReflectParams {
    /// Scope of reflection (Phase 4).
    scope: Option<String>,
    /// Time window (Phase 4).
    window: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct StatsParams {
    /// Return detailed stats (Phase 4).
    detail: Option<bool>,
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

        let memory = self.service.store(req).await.map_err(map_error)?;
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "stored": true,
            "id": memory.id,
            "type": memory.memory_type,
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
            include_stale: p.include_stale,
            include_invalidated: p.include_invalidated,
            time_range: p.time_range.map(|tr| TimeRange {
                start: tr.start,
                end: tr.end,
            }),
        };

        let results = self.service.recall(req).await.map_err(map_error)?;
        let output: Vec<serde_json::Value> = results
            .into_iter()
            .map(|(mem, score)| {
                serde_json::json!({
                    "id": mem.id,
                    "type": mem.memory_type,
                    "content": mem.content,
                    "subject": mem.subject,
                    "predicate": mem.predicate,
                    "object": mem.object,
                    "score": score,
                    "entities": mem.entities,
                    "created_at": mem.created_at.to_rfc3339(),
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
        let resp = self.service.forget(&req).map_err(map_error)?;
        Ok(serde_json::to_string_pretty(&resp).unwrap_or_default())
    }

    #[tool(
        name = "reflect",
        description = "Audit memory health. Surfaces stale memories, contradictions, and low-access candidates for cleanup."
    )]
    async fn reflect(&self, Parameters(p): Parameters<ReflectParams>) -> Result<String, ErrorData> {
        let req = ReflectRequest {
            scope: p.scope,
            window: p.window,
        };
        let resp = self.service.reflect(req).map_err(map_error)?;
        Ok(serde_json::to_string_pretty(&resp).unwrap_or_default())
    }

    #[tool(
        name = "stats",
        description = "Overview of the memory system. Shows total count, recent memories, and items needing attention. Call this at conversation start to orient yourself."
    )]
    async fn stats(&self, Parameters(p): Parameters<StatsParams>) -> Result<String, ErrorData> {
        let req = StatsRequest { detail: p.detail };
        let resp = self.service.stats(req).await.map_err(map_error)?;
        Ok(serde_json::to_string_pretty(&resp).unwrap_or_default())
    }
}

#[tool_handler]
impl ServerHandler for FerrexServer {}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    let db_path = cli.db_path.unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".ferrex")
            .join("ferrex.db")
    });

    let config = FerrexConfig {
        qdrant_url: cli.qdrant_url,
        qdrant_bin: cli.qdrant_bin,
        qdrant_port: cli.qdrant_port,
        model_tier: cli.model_tier,
        namespace: cli.namespace,
        db_path,
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

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

            tokio::select! {
                result = running.waiting() => {
                    result.map_err(|e| eyre::eyre!("MCP server error: {e}"))?;
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("received SIGINT, shutting down");
                }
            }

            if let Some(ref mut sc) = sidecar {
                sc.shutdown();
            }
            Ok(())
        })
}
