# ferrex Future Improvements

Features deferred from v1 to keep the initial implementation simple and measurable. Each improvement should be gated on evidence from `stats` instrumentation and retrieval quality measurements — not added speculatively.

## Retrieval Channels

### kNN Link Graph
**What:** Precomputed semantic proximity links between memories. At insert time, compute top-5 neighbors (cosine >= 0.7), store bidirectional links in SQLite. At query time, expand top vector results through their links.

**Why deferred:** Unproven contribution in isolation. Query-time second-pass search (re-query seeded by top-1 result embedding) achieves similar "find things similar to my best match" behavior with zero insert cost or link maintenance.

**When to add:** If `stats` shows recall misses where the correct memory is semantically adjacent to a returned result but not in the top-K. Measure recall@10 with and without link expansion on real workload.

**Design:** See original design doc Decision #1 (bidirectional insert + cap at 10). SQLite `memory_links` table, links queried as additional retrieval channel merged via RRF.

**References:** Hindsight's 4-network approach uses precomputed kNN; arXiv:2502.13862 benchmarks petgraph at scale.

### Graph Expansion (Knowledge Graph as Retrieval Channel)
**What:** Use entity/relation graph as a retrieval channel. At query time: detect entities in query, find entity nodes, traverse 1-2 hops, collect connected memories, merge into RRF.

**Why deferred:** GraphRAG-Bench (June 2025) showed graph retrieval is 13.4% *less accurate* than vanilla RAG on single-hop factoid queries. StructMemEval (Feb 2026) showed simple embedding retrieval outperforms complex memory structures on most benchmarks. The graph channel adds value primarily for multi-hop relational queries ("what caused X which depends on Y") — measure first whether these queries are common in practice.

**When to add:** If `stats` shows frequent queries with relational/causal intent (detectable via keyword patterns: "caused", "depends on", "related to") that vector + BM25 fails to answer. Requires the `relate` tool (below) to populate the graph.

**Design:** petgraph in-memory cache loaded from SQLite on startup. SQLite as source of truth. 1-2 hop traversal with boosted weight for causal predicates. Merged as a third RRF channel.

**Scaling note:** petgraph is fine for thousands of entities. At 50K+ entities, benchmark SQLite indexed queries vs petgraph in-memory. arXiv:2502.13862 shows petgraph is 87x slower than alternatives for batch updates but fine for reads at moderate scale.

### Adaptive Query Routing
**What:** Rule-based classifier that adjusts retrieval channel weights per query. BM25-heavy for exact identifiers, vector-heavy for semantic queries, graph-heavy for relational queries, temporal mode for date-scoped queries.

**Why deferred:** No data yet on query patterns ferrex will see. Equal-weight vector + BM25 is a safe default. Premature routing risks degrading queries that don't fit the classifier's heuristics.

**When to add:** After collecting query logs from real usage. Analyze which channel contributes most per query type. Build routing rules from observed data, not assumptions.

**Design:** Zero-latency regex + heuristic classifier. Channel weight map per query class. Easily added as a pre-step in retrieval pipeline.

### Two-Phase Temporal Retrieval
**What:** When query contains temporal markers, first query SQLite for time-windowed memories (cheap), then compute vector similarity only for those candidates (expensive). Avoids full-corpus vector search for time-scoped queries.

**Why deferred:** Qdrant payload filtering on timestamps achieves similar results with a single query. The two-phase approach is an optimization for large corpora where the time filter is highly selective.

**When to add:** If vector search latency becomes noticeable (>50ms) and time-scoped queries are common. Simple to implement — SQLite query + filtered Qdrant search.

## Tools

### `relate` Tool
**What:** Explicit relationship creation between entities. `relate(subject, predicate, object, weight)`. Supports causal predicates (`caused_by`, `enables`, `prevents`).

**Why deferred:** Without graph expansion as a retrieval channel, explicit relations have no retrieval path. Entity metadata on memories + payload filtering covers basic entity-based queries.

**When to add:** Together with graph expansion retrieval channel. The two are a package — relations without graph retrieval are inert data.

**Design:** SQLite relation table, `relate` tool as 6th MCP tool. Causal predicates get boosted retrieval weight.

### `reflect` — Episodic Clustering + Semantic Promotion
**What:** Cluster similar episodic memories by similarity, propose semantic facts for agent confirmation. "These 3 memories over 2 weeks involve connection-pool + api-server. Consider storing a semantic fact."

**Why deferred:** v1 `reflect` handles staleness audit + contradiction detection (the highest-value use cases). Episodic clustering requires either LLM integration or sophisticated unsupervised methods to produce useful suggestions. Per design Decision #4, ferrex stays LLM-free — the agent does the reasoning.

**When to add:** When episodic memory count grows large enough that manual promotion is impractical. The clustering infrastructure (cosine similarity grouping, shared entity detection) is straightforward; the challenge is producing suggestions the agent actually acts on.

**Design:** Cluster by embedding similarity (threshold TBD from data), detect shared entities, return structured suggestions. Agent confirms via `store(type: "semantic", ...)`. Optional Ollama integration for automated triple extraction.

### `reflect` — Batch Confirm/Reject
**What:** Instead of reflect returning suggestions that require individual `store`/`forget` calls, return a batch operation the agent can approve with a single `confirm(ids: [...])` call.

**Why deferred:** v1 reflect is read-only (audit). Batch operations add transactional complexity.

**When to add:** When reflect returns actionable suggestions (episodic clustering). The multi-roundtrip problem only manifests with the promotion workflow.

## Entity Resolution

### Fuzzy String Matching
**What:** SequenceMatcher ratio > 0.85 against existing entity names and aliases. Catches "postgres" ↔ "postgresql".

**Why deferred:** v1 uses normalization only (lowercase, trim, collapse). Good enough for most cases. Fuzzy matching adds false positive risk.

**When to add:** If `stats` shows entity fragmentation (many entities with similar names but different IDs). Check entity table for near-duplicates periodically.

### Embedding-Based Entity Resolution
**What:** Embed entity names, cosine > 0.92 → merge, 0.80-0.92 → alias candidates surfaced in reflect.

**Why deferred:** Requires embedding every entity name (small cost per entity but adds insert latency). v1 normalization handles the obvious cases.

**When to add:** Together with fuzzy matching, when entity fragmentation is measured. The full layered pipeline (normalize → fuzzy → embedding → manual review) is well-designed — just not needed at launch.

### Alias Table
**What:** Each entity has a canonical name + list of aliases. All lookups check aliases first.

**Why deferred:** Aliases are the output of fuzzy + embedding resolution. Without those resolution steps, aliases have no source.

**When to add:** Together with the resolution pipeline above.

## Embedding

### Embedding Prefix Format Benchmarking
**What:** Current `[type | namespace | date]` prefix may use tokens out-of-distribution for BGE-base. Alternatives: (a) no prefix, (b) natural language prefix ("This is a record of an event in the api-server project from April 2026:"), (c) Qdrant payload filtering instead of embedding the metadata.

**Why important:** HeteRAG (arXiv:2504.10529) argues for decoupling retrieval and generation representations. ECIR 2026 paper shows dual-encoder with unified embeddings matches text prefixing. The current approach is the cheapest possible improvement — but it may not be an improvement at all.

**When to do:** Early in v1 usage. A/B test with a handful of real memories. Compare recall@5 with and without prefix on representative queries.

### BGE-M3 Unified Model
**What:** BGE-M3 outputs dense + sparse + ColBERT from a single forward pass. Could replace separate BGE-base embedding + Qdrant BM25 tokenization.

**When to evaluate:** After v1 is stable. Benchmark retrieval quality and latency against the current two-model approach.

## Infrastructure

### petgraph In-Memory Graph Cache
**What:** Load entity/relation graph into petgraph on startup for sub-microsecond traversal.

**Why deferred:** SQLite with proper indexes provides sub-millisecond graph queries, which is invisible next to 200ms+ reranking latency. petgraph adds a consistency problem (SQLite as source of truth, petgraph as stale cache) and startup cost (rebuilding graph on every launch).

**When to add:** If graph traversal becomes a retrieval channel AND SQLite query latency is measurable in the pipeline. At 50K+ entities, benchmark SQLite vs petgraph.

### Community Detection (Leiden Algorithm)
**What:** Detect entity clusters for global queries ("summarize everything about the api-server project").

**Why deferred:** Requires a populated knowledge graph with relations. Pre-requisite: `relate` tool + graph retrieval channel.

**When to add:** Late v2, after graph infrastructure is proven.

### Optional LLM Integration (Ollama)
**What:** Local LLM for automated episodic→semantic promotion, entity extraction from free text, and advanced reflect operations.

**Why deferred:** ferrex's core value proposition is LLM-free operation. LLM integration is an optional enhancement for users who want automation.

**When to add:** After manual reflect workflow is validated. Ollama as optional dependency, no cloud API calls.

## Measurement Plan

All v2 features should be gated on data. The `stats` tool and retrieval instrumentation from Phase 5 provide the foundation:

| Metric | What it tells you | Enables |
|---|---|---|
| **Recall miss rate** | Queries where the correct memory exists but isn't in top-5 | kNN links, graph expansion, adaptive routing |
| **Query pattern distribution** | Ratio of exact/semantic/relational/temporal queries | Adaptive routing weights |
| **Entity fragmentation** | Count of near-duplicate entities | Fuzzy + embedding entity resolution |
| **Episodic memory growth** | Rate of episodic accumulation without promotion | Reflect clustering + semantic promotion |
| **Channel contribution** | Per-query: did vector or BM25 find the winning result? | Channel weight tuning, additional channels |
| **Reranking lift** | Score delta between RRF rank and final reranked position | Reranker model selection |
| **Embedding prefix impact** | Retrieval quality with/without prefix | Prefix format decision |
