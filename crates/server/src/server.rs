use std::sync::Arc;

use ferrex_core::{
    CoreError, ForgetRequest, MemoryService, RecallRequest, ReflectRequest, StatsRequest,
    StoreRequest,
};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};

use crate::hint;
use crate::params::{ForgetParams, RecallParams, ReflectParams, StatsParams, StoreParams};

#[derive(Clone)]
pub struct FerrexServer {
    service: Arc<MemoryService>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl FerrexServer {
    pub fn new(service: Arc<MemoryService>) -> Self {
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
            cascade: None,
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
impl ServerHandler for FerrexServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}
