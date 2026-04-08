use std::collections::HashSet;
use std::sync::Arc;

use ferrex_store::MetadataStore;

use super::MemoryService;
use crate::error::CoreError;
use crate::ops_buffer::{AuditReport, BackfillReport};

impl MemoryService {
    pub async fn audit_reconcile(
        &self,
        fix: bool,
        fix_limit: u64,
    ) -> Result<AuditReport, CoreError> {
        let qdrant_ids = self
            .vector_store
            .scroll_all_ids(&self.config.namespace)
            .await?;
        let qdrant_set: HashSet<String> = qdrant_ids.iter().map(ToString::to_string).collect();

        let sqlite_ids = self
            .metadata_store
            .list_all_memory_ids(&self.config.namespace)
            .await?;
        let sqlite_set: HashSet<String> = sqlite_ids.into_iter().collect();

        let qdrant_only: Vec<String> = qdrant_set.difference(&sqlite_set).cloned().collect();
        let sqlite_only: Vec<String> = sqlite_set.difference(&qdrant_set).cloned().collect();

        if fix {
            if qdrant_only.len() as u64 > fix_limit {
                return Err(CoreError::Validation(format!(
                    "audit refuses to auto-fix {} qdrant-only ids (limit {fix_limit})",
                    qdrant_only.len(),
                )));
            }
            let uuids: Vec<uuid::Uuid> = qdrant_only
                .iter()
                .filter_map(|s| uuid::Uuid::parse_str(s).ok())
                .collect();
            self.vector_store
                .delete_by_ids(&self.config.namespace, &uuids)
                .await?;
            Ok(AuditReport {
                sqlite_only,
                fixed: qdrant_only,
                qdrant_only: vec![],
            })
        } else {
            Ok(AuditReport {
                sqlite_only,
                qdrant_only,
                fixed: vec![],
            })
        }
    }

    pub async fn backfill_normalized_predicates(
        &self,
        namespace: Option<&str>,
        dry_run: bool,
    ) -> Result<BackfillReport, CoreError> {
        let rows = self
            .metadata_store
            .semantic_rows_missing_normalized_predicate(namespace)
            .await?;
        let mut report = BackfillReport {
            scanned: rows.len() as u64,
            updated: 0,
        };
        for row in rows {
            let normalizer = self
                .normalizers
                .get(&row.namespace)
                .cloned()
                .unwrap_or_else(|| Arc::clone(&self.default_normalizer));
            let Some(pred) = row.predicate.as_deref() else {
                continue;
            };
            let normalized = normalizer.normalize(pred);
            if normalized == pred {
                continue;
            }
            if dry_run {
                report.updated += 1;
                continue;
            }
            self.metadata_store
                .set_normalized_predicate(&row.id, &normalized)
                .await?;
            report.updated += 1;
        }
        Ok(report)
    }

    pub async fn list_pending_ops(&self) -> Result<Vec<ferrex_store::PendingOp>, CoreError> {
        Ok(self.metadata_store.list_pending_ops().await?)
    }

    pub async fn list_completed_ops(
        &self,
        status: Option<&str>,
        limit: usize,
        since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<ferrex_store::CompletedOp>, CoreError> {
        Ok(self
            .metadata_store
            .list_completed_ops(status, limit, since)
            .await?)
    }

    pub async fn prune_journal(&self) -> Result<(), CoreError> {
        self.metadata_store
            .prune_completed_ops(1000, chrono::Duration::days(7))
            .await?;
        Ok(())
    }
}
