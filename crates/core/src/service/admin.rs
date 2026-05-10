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
        let mut namespaces: HashSet<String> = HashSet::new();
        namespaces.extend(self.metadata_store.list_namespaces().await?);
        namespaces.extend(self.vector_store.list_namespaces().await?);
        namespaces.insert(self.config.namespace.clone());
        let mut namespaces: Vec<String> = namespaces.into_iter().collect();
        namespaces.sort();

        let mut sqlite_only_all: Vec<String> = Vec::new();
        let mut qdrant_only_all: Vec<String> = Vec::new();
        for namespace in &namespaces {
            let qdrant_ids = self.vector_store.scroll_all_ids(namespace).await?;
            let qdrant_set: HashSet<String> = qdrant_ids.iter().map(ToString::to_string).collect();
            let sqlite_ids = self.metadata_store.list_all_memory_ids(namespace).await?;
            let sqlite_set: HashSet<String> = sqlite_ids.into_iter().collect();

            sqlite_only_all.extend(sqlite_set.difference(&qdrant_set).cloned());
            qdrant_only_all.extend(qdrant_set.difference(&sqlite_set).cloned().map(|id| {
                // Tag qdrant-only entries with their namespace so the fix pass
                // can route deletes to the right collection.
                format!("{namespace}:{id}")
            }));
        }

        if fix {
            if qdrant_only_all.len() as u64 > fix_limit {
                return Err(CoreError::Validation(format!(
                    "audit refuses to auto-fix {} qdrant-only ids (limit {fix_limit})",
                    qdrant_only_all.len(),
                )));
            }
            // Group ids by namespace, then issue one delete per collection.
            let mut by_ns: std::collections::HashMap<String, Vec<uuid::Uuid>> =
                std::collections::HashMap::new();
            for tagged in &qdrant_only_all {
                let Some((ns, id)) = tagged.split_once(':') else {
                    continue;
                };
                if let Ok(uuid) = uuid::Uuid::parse_str(id) {
                    by_ns.entry(ns.to_string()).or_default().push(uuid);
                }
            }
            for (ns, uuids) in &by_ns {
                self.vector_store.delete_by_ids(ns, uuids).await?;
            }
            // Strip the namespace prefix from the report so the output stays
            // back-compat for callers that just want the bare ids.
            let fixed: Vec<String> = qdrant_only_all
                .iter()
                .filter_map(|tagged| tagged.split_once(':').map(|(_, id)| id.to_string()))
                .collect();
            Ok(AuditReport {
                sqlite_only: sqlite_only_all,
                fixed,
                qdrant_only: vec![],
            })
        } else {
            // Strip the namespace prefix for unfixed reports too.
            let qdrant_only: Vec<String> = qdrant_only_all
                .iter()
                .filter_map(|tagged| tagged.split_once(':').map(|(_, id)| id.to_string()))
                .collect();
            Ok(AuditReport {
                sqlite_only: sqlite_only_all,
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
