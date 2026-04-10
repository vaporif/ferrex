use ferrex_store::MetadataStore;

use super::MemoryService;
use crate::error::CoreError;
use crate::types::{
    TaxonomyEntityEntry, TaxonomyPredicateEntry, TaxonomyRequest, TaxonomyResponse,
};

const DEFAULT_TAXONOMY_LIMIT: usize = 10;
const MAX_TAXONOMY_LIMIT: usize = 100;

impl MemoryService {
    #[tracing::instrument(name = "taxonomy", skip_all, fields(namespace))]
    pub async fn taxonomy(
        &self,
        req: TaxonomyRequest,
    ) -> Result<TaxonomyResponse, CoreError> {
        let scope_given = req.namespace.is_some();
        let namespace = req
            .namespace
            .unwrap_or_else(|| self.config.namespace.clone());
        tracing::Span::current().record("namespace", namespace.as_str());

        let limit = req
            .limit
            .unwrap_or(DEFAULT_TAXONOMY_LIMIT)
            .min(MAX_TAXONOMY_LIMIT);
        #[allow(clippy::cast_possible_wrap)]
        let limit_i64 = limit as i64;

        // Four reads, four reader-pool slots.
        let by_type_fut = self.metadata_store.memory_count_by_type(&namespace);
        let top_entities_fut = self.metadata_store.top_entities(&namespace, limit_i64);
        let top_predicates_fut = self.metadata_store.top_predicates(&namespace, limit_i64);
        let all_namespaces_fut = async {
            if scope_given {
                Ok(None)
            } else {
                self.metadata_store.list_namespaces().await.map(Some)
            }
        };
        let (by_type, top_entities_raw, top_predicates_raw, all_namespaces) = tokio::try_join!(
            by_type_fut,
            top_entities_fut,
            top_predicates_fut,
            all_namespaces_fut,
        )?;

        let total_memories: u64 = by_type.values().sum();
        let top_entities = top_entities_raw
            .into_iter()
            .map(|(name, memory_count)| TaxonomyEntityEntry { name, memory_count })
            .collect();
        let top_predicates = top_predicates_raw
            .into_iter()
            .map(|(predicate, count)| TaxonomyPredicateEntry { predicate, count })
            .collect();

        Ok(TaxonomyResponse {
            namespace,
            total_memories,
            by_type,
            top_entities,
            top_predicates,
            all_namespaces,
        })
    }
}
