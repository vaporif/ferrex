# ferrex Implementation Roadmap

## Phase 1: Foundation

**Delivers**: Working MCP server over stdio with store, recall, stats (brief) tools. Local-first with Qdrant sidecar and ONNX embeddings.

- 4-crate workspace: ferrex-server (thin MCP shell), ferrex-core (library API), ferrex-embed, ferrex-store
- fastembed embedding (BGE-base-en-v1.5 default, configurable tiers)
- Qdrant dense vector write + nearest-neighbor search
- SQLite metadata via MetadataStore trait (Postgres-ready)
- Full entity resolution pipeline (normalize → fuzzy → embedding → alias)
- Qdrant sidecar lifecycle (PID file, --qdrant-url external mode)
- Memory type auto-detection (semantic only; procedural requires explicit type)
- Namespace support, CLI config via clap + env vars
- Nix build with onnxruntime.nix

**Spec**: `docs/superpowers/specs/2026-04-03-phase1-foundation-design.md`

## Phase 2: Reranking + Hybrid Retrieval

**Delivers**: Better recall quality. Cross-encoder reranking is the primary win — promotes the best results from a larger candidate pool. BM25 is a secondary addition that helps with exact keyword/identifier matches.

- Cross-encoder reranking: fastembed's `bge-reranker-base` on top-20 candidates
- Add BM25 sparse vectors to Qdrant (server-side tokenization + IDF via `Modifier::Idf` on `SparseVectorParams`)
- Hybrid recall: prefetch dense + sparse, fuse via Qdrant `Fusion::Rrf` (k=60) in one API call
- Multiplicative recency boosts: `final_score = rerank_score x recency_boost`
- Type-specific recency: episodic (half-life 30d, range 1.0-1.3), semantic (half-life 180d, range 1.0-1.15), procedural (none)

**Note on BM25**: ferrex memories are short (50-300 chars), so BGE-base already captures most keyword signal. BM25's main value is for exact identifiers and entity names. The cross-encoder reranker is where most retrieval quality improvement comes from.

**Not in Phase 2**: Staleness filtering/annotation (requires Phase 4 scoring machinery), temporal proximity boost (underspecified, recency boost covers the time signal).

**Migration**: Not needed — not live yet. Collections are recreated with both dense and sparse config.

## Phase 3: Conflict Resolution + Deduplication

**Delivers**: Semantic facts that auto-update when knowledge changes. Deduplication prevents bloat. Entity conflicts surface for review.

- Deduplication on store: embed incoming → search same-type → cosine > configurable threshold (default 0.95) → reject
- Conflict detection for semantic facts: match (subject, normalized_predicate) → compare objects → invalidate old with `t_invalid`
- Predicate normalization: static synonym map + strsim fuzzy match (> 0.85)
- `forget` tool (ID-based deletion only)
- `supersedes` param on store (bypass dedup, invalidate target)

**Depends on**: Phase 1 SQLite schema (t_valid/t_invalid columns already present).

## Phase 4: Memory Lifecycle

**Delivers**: Self-maintaining memory. Stale facts surface for review. Stats give full diagnostics.

- `reflect` tool: staleness audit + contradiction alerts (no episodic clustering — that's v2)
- `stats` detailed mode: counts by type, staleness distribution, storage size, entity count
- Staleness scoring: multi-signal function of age, last_accessed, last_validated, access_count, type
- Configurable thresholds per type, overridable per namespace via ferrex.toml
- Retrieval vs validation separation enforced (last_accessed != last_validated)
- Decay scoring with half-life per type
- Eviction priority: superseded semantics → stale unreferenced episodics → aging episodics
- Freshness metadata on every recall result

**Depends on**: Phase 1 SQLite schema (last_accessed, last_validated, access_count already populated).

## Phase 5: Polish

**Delivers**: Production readiness. Verified retrieval quality. CI pipeline.

- Semantic caching (LRU on recall, keyed by embedding hash)
- Production Nix package (wrapProgram with ORT_DYLIB_PATH, CI build/test pipeline)
- Integration test suite against real Qdrant
- Golden set retrieval test (20 memories, 10 queries, target recall@3 >= 0.7)
- MCP Inspector validation (all tools, parameter schemas, error formats)
- Retrieval quality instrumentation (for measuring v2 improvements)

## Future (v2, evidence-gated)

All v2 features should be gated on data from stats instrumentation and retrieval quality measurements. See `docs/design/future-improvements.md` for full details.

- kNN link graph as retrieval channel
- Knowledge graph traversal as retrieval channel
- Adaptive query routing (BM25-heavy for identifiers, vector-heavy for semantic queries)
- `relate` tool for explicit entity relationships
- `reflect` episodic clustering + semantic promotion
- Optional LLM integration (Ollama) for automated triple extraction
- Community detection (Leiden algorithm)
- Postgres backend for MetadataStore trait
