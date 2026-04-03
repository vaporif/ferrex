# ferrex: RAG Memory MCP Server

## Overview

ferrex is a local-first MCP server that provides intelligent long-term memory for AI agents. It combines vector search, BM25 keyword matching, and a lightweight knowledge graph into a unified retrieval system — exposed through memory-typed tools that agents interact with naturally.

The goal is not another vector store with an MCP wrapper. It is a memory system that understands temporal facts, resolves contradictions, manages its own lifecycle, and retrieves context through three complementary signal paths fused into one result.

## Design Principles

1. **Minimal ops** — one Rust binary + Qdrant sidecar. No Python, no Docker compose stacks, no cloud accounts.
2. **Memory semantics, not storage semantics** — the API speaks in episodic/semantic/procedural terms, not vectors and indexes.
3. **Retrieval quality over storage volume** — hybrid search with reranking by default. Better to return 3 excellent results than 10 mediocre ones.
4. **Temporal awareness** — every fact has a validity timeline. Contradictions are detected and resolved, not silently accumulated.
5. **Staleness-aware** — stale memories are detected, flagged, and decayed. The system never silently returns outdated facts without signaling freshness.
6. **Local-first** — embeddings generated locally (ONNX), data stored locally, no network calls required for core operation.

## Competitive Positioning

The landscape (as of April 2026):

| System | Approach | Limitation ferrex solves |
|---|---|---|
| **mem0** | LLM-based extraction → vector store | Summarization destroys detail; ~2-3% recall on long contexts |
| **Hindsight** | 4-network retrieval, Python/FastAPI, Postgres | No adaptive retrieval; SQL-join-based graph hits scaling limits |
| **Cognee** | KG + vector, Python, 30+ connectors | Heavy Python stack, not embeddable |
| **Zep/Graphiti** | Temporal KG, Neo4j | Killed self-hosted; cloud-only (Graphiti lib is OSS) |
| **Letta/MemGPT** | Self-editing memory, agent controls recall | No knowledge graph, no hybrid search |
| **memory-mcp-rs** | Rust + SQLite KG | No vector search, no embeddings |

ferrex's unique position: **Rust-native + Qdrant sidecar + hybrid search (vector + BM25 + KG) + temporal validity + staleness safeguards + MCP-native**.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    MCP Transport                     │
│              (rmcp, stdio / SSE)                     │
└────────────────────────┬────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────┐
│                    Tool Router                       │
│     store / recall / forget / reflect / relate       │
└────────────────────────┬────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────┐
│                  Memory Router                       │
│  classifies queries by complexity, routes to         │
│  appropriate retrieval strategy                      │
└───────┬────────────────┬────────────────┬───────────┘
        │                │                │
┌───────▼───────┐ ┌──────▼──────┐ ┌───────▼───────┐
│  Ingestion    │ │  Retrieval  │ │   Lifecycle   │
│  Pipeline     │ │  Engine     │ │   Manager     │
└───────┬───────┘ └──────┬──────┘ └───────┬───────┘
        │                │                │
┌───────▼────────────────▼────────────────▼───────────┐
│                   Storage Layer                      │
│  ┌──────────────────────────────┐                    │
│  │  Qdrant (sidecar process)    │                    │
│  │  • Dense vector index (HNSW) │                    │
│  │  • Sparse/BM25 index         │                    │
│  │  • Payload filtering          │                    │
│  └──────────────────────────────┘                    │
│  ┌──────────────────────────────┐                    │
│  │  SQLite (in-process)          │                    │
│  │  • Knowledge graph tables     │                    │
│  │  • kNN link cache             │                    │
│  │  • Temporal validity tracking │                    │
│  │  • Metadata, access counts    │                    │
│  │  • Staleness scores           │                    │
│  └──────────────────────────────┘                    │
│  ┌──────────────────────────────┐                    │
│  │  petgraph (in-memory cache)   │                    │
│  │  • Entity/relation traversal  │                    │
│  │  • Loaded from SQLite on boot │                    │
│  └──────────────────────────────┘                    │
└─────────────────────────────────────────────────────┘
        │
┌───────▼─────────────────────────────────────────────┐
│                  Embedding Engine                     │
│              (fastembed, ONNX local)                  │
│  embedding: configurable (see Model Tiers)           │
│  reranking: configurable (see Model Tiers)           │
└─────────────────────────────────────────────────────┘
```

### Qdrant Sidecar

Qdrant runs as a managed subprocess. ferrex starts it on launch and stops it on shutdown. The Rust binary communicates via gRPC using `qdrant-client`. This trades pure single-binary for access to Qdrant's full feature set (HNSW, sparse vectors, payload filtering, named vectors). The sidecar is invisible to the user — ferrex manages its lifecycle.

For service mode, the same `qdrant-client` code points to a remote Qdrant URL instead of localhost — no code change needed.

## Memory Types

All stored facts use a **self-contained format**: each memory must be independently meaningful without surrounding context. The recommended structure is "what | when | where | who | why" — ensuring that a retrieved memory makes sense even without its neighbors. This is critical for retrieval quality (learned from Hindsight's approach).

### Episodic Memory
Records of specific events and interactions. Timestamped, contextual, append-only.

```
{
  "type": "episodic",
  "content": "user debugged a deadlock in the connection pool by switching to tokio::sync::Semaphore | 2026-04-03 | api-server project | outcome: success",
  "context": { "task": "bug-fix", "project": "api-server", "outcome": "success" },
  "timestamp": "2026-04-03T10:30:00Z",
  "entities": ["connection-pool", "tokio::sync::Semaphore", "api-server"],
  "causal_links": [
    { "predicate": "caused_by", "target": "deadlock bug", "weight": 0.9 }
  ]
}
```

- **Storage**: vector embedding + BM25 index + metadata + kNN links (precomputed at insert time)
- **Retrieval**: temporal + similarity search + kNN link expansion
- **Lifecycle**: decays after configurable TTL unless accessed. Candidates for consolidation into semantic memory via `reflect`.

### Semantic Memory
Stable facts, concepts, entity relationships. The knowledge graph lives here.

```
{
  "type": "semantic",
  "subject": "api-server",
  "predicate": "uses",
  "object": "tokio 1.38",
  "confidence": 0.95,
  "source": "episodic:abc123",
  "t_valid": "2026-04-01T00:00:00Z",
  "t_invalid": null
}
```

- **Storage**: vector embedding + BM25 index + knowledge graph node/edge + metadata
- **Retrieval**: exact match + semantic search + graph traversal
- **Lifecycle**: never auto-decays. Updated via upsert with conflict resolution. Old values archived with `t_invalid` timestamp set.
- **Temporal validity**: every semantic fact has `t_valid` (when it became true) and `t_invalid` (when it stopped being true, null if current). Queries default to current facts only. Historical queries can specify a time range to include invalidated facts. (Adopted from Zep/Graphiti's bi-temporal model, which scored 94.8% on DMR benchmark.)

### Procedural Memory
Workflows, heuristics, learned strategies. Versioned.

```
{
  "type": "procedural",
  "name": "deploy-to-staging",
  "conditions": ["branch is main", "tests pass"],
  "steps": ["build release", "push to registry", "apply k8s manifest"],
  "version": 3
}
```

- **Storage**: vector embedding + BM25 index + metadata
- **Retrieval**: pattern matching on conditions + similarity
- **Lifecycle**: versioned. Old versions kept for rollback.

## MCP Tools API

Tool count is kept low (6 tools) to minimize context window tax. Research shows MCP tools can consume 16%+ of context — fewer, richer tools are better.

### Tool Descriptions as Agent Instructions

MCP tool descriptions are loaded into the agent's context at session start. They are the primary mechanism for guiding agent behavior — no hooks, no system prompt injection needed. The descriptions below are carefully crafted to serve as both API documentation *and* implicit behavioral instructions.

### `store`

**MCP description** (what the agent sees):
> Store a memory for long-term recall. Call this whenever you learn something worth remembering: new facts about the user or project, decisions made, problems solved, workflows discovered, or corrections to previous knowledge. Use type "episodic" for events and interactions, "semantic" for stable facts and entity relationships, "procedural" for workflows and learned strategies. Write self-contained memories — each should make sense on its own without surrounding context. Include relevant entities to build the knowledge graph. If this updates a previously known fact, the system detects and resolves the contradiction automatically.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `type` | string | yes | `"episodic"`, `"semantic"`, or `"procedural"` |
| `content` | string | yes* | What happened (episodic) or the procedure steps (procedural). Self-contained format recommended: "what \| when \| where \| who \| why" |
| `subject` | string | yes* | The entity this fact is about (semantic only) |
| `predicate` | string | yes* | The relationship or property (semantic only) |
| `object` | string | yes* | The value or target entity (semantic only) |
| `confidence` | float | no | 0.0-1.0, defaults to 1.0 |
| `source` | string | no | Provenance (memory ID, URL, etc.) |
| `entities` | string[] | no | Entity names to extract/link in the knowledge graph |
| `relations` | object[] | no | Explicit relations `[{subject, predicate, object, weight}]`. Supports causal predicates: `caused_by`, `enables`, `prevents` |
| `context` | object | no | Structured context (task, project, outcome, etc.) |
| `supersedes` | string | no | Memory ID to explicitly replace (skips similarity check) |

*Required fields depend on `type`: episodic/procedural require `content`; semantic requires `subject`+`predicate`+`object`.

**On store, the ingestion pipeline**:
1. **Context-enriched embedding**: prepend metadata before embedding (see Embedding Strategy)
2. **Chunking** (if needed): apply type-aware chunking (see Chunking Strategy)
3. Embed via fastembed → write to Qdrant (dense + sparse/BM25 vectors)
4. Compute **top-5 kNN links** against existing memories (similarity >= 0.7, bidirectional — also update neighbors' link lists, cap at 10 per memory). Stored in SQLite `memory_links` table.
5. Extract/link entities in knowledge graph (with alias resolution — see Entity Resolution)
6. For semantic type: run conflict detection (see Conflict Resolution)
7. For procedural type: create new version if name already exists

### `recall`

**MCP description** (what the agent sees):
> Search long-term memory. Call this at the START of every conversation to load relevant context about the user, project, and prior decisions. Also call whenever you need to remember something: past discussions, known facts, established workflows, or entity relationships. Returns results ranked by relevance with freshness metadata — check the staleness field to gauge how current each memory is.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | yes | What to search for |
| `types` | string[] | no | Filter by memory type: `["episodic", "semantic", "procedural"]` |
| `limit` | int | no | Max results, default 5 |
| `time_range` | object | no | `{after: "...", before: "..."}` for temporal filtering |
| `entities` | string[] | no | Filter to memories involving these entities |
| `include_stale` | bool | no | Include memories flagged as potentially stale, default false |
| `include_invalidated` | bool | no | Include semantic facts with `t_invalid` set (historical queries), default false |

**Retrieval pipeline** (see Retrieval Pipeline Detail for full walkthrough):
1. Embed query via fastembed
2. Query routing — classify query and weight retrieval channels (see Adaptive Retrieval)
3. Parallel search: vector top-K, BM25 top-K, kNN link expansion, graph expansion (if entities detected)
4. Reciprocal Rank Fusion (k=60) to merge results
5. Cross-encoder reranking with **multiplicative** recency and temporal proximity boosts
6. Staleness annotation on each result
7. Return top-N with scores, provenance, and freshness metadata

**Each result includes freshness metadata:**
```json
{
  "id": "mem_12",
  "content": "...",
  "score": 0.94,
  "freshness": {
    "age_days": 45,
    "last_accessed": "2026-02-17T...",
    "last_validated": "2026-03-01T...",
    "access_count": 7,
    "staleness": "fresh"
  }
}
```

`staleness` field values: `"fresh"`, `"aging"` (approaching staleness threshold), `"stale"` (exceeded threshold, returned only if `include_stale=true`), `"superseded"` (a newer fact exists for the same subject+predicate).

### `forget`

**MCP description** (what the agent sees):
> Delete or invalidate memories that are no longer accurate or relevant. Use this when you discover a memory is wrong, outdated, or the user asks you to forget something. Provide specific memory IDs for targeted deletion, or a query to find and remove matching memories.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `ids` | string[] | no | Specific memory IDs to delete |
| `query` | string | no | Delete memories matching this query (requires confirmation) |
| `cascade` | bool | no | Also remove graph edges involving forgotten entities |

### `reflect`

**MCP description** (what the agent sees):
> Consolidate and audit memories. Call this periodically (e.g., end of a long session or weekly) to: extract recurring patterns from recent events into stable facts, surface stale memories that need review, and detect contradictions between active facts. Review the results and confirm or discard proposed changes.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `scope` | string | no | Limit reflection to a project/topic |
| `window` | string | no | Time window to reflect over, default "7d" |

Returns:
- Proposed semantic facts extracted from episodic patterns (agent confirms or discards)
- List of stale/unvalidated memories that need review
- Contradiction alerts (multiple active facts for same subject+predicate)

### `relate`

**MCP description** (what the agent sees):
> Create a relationship between two entities in the knowledge graph. Use this when you discover how things connect: dependencies, causation, composition, or other relationships. Supports causal predicates (caused_by, enables, prevents) which boost retrieval for "why" and "how" questions.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `subject` | string | yes | Source entity |
| `predicate` | string | yes | Relationship type (including causal: `caused_by`, `enables`, `prevents`) |
| `object` | string | yes | Target entity |
| `weight` | float | no | Relationship strength, 0.0-1.0, default 1.0 |

### `stats`

**MCP description** (what the agent sees):
> Memory system health and metrics. Call this to understand memory state: total count by type, staleness distribution, contradictions detected, storage usage. Useful before deciding whether to run reflect or forget.

Returns: total memories by type, staleness distribution (fresh/aging/stale counts), conflict count, most/least accessed, storage size, graph node/edge counts, kNN link count.

## Knowledge Graph

The graph stores entities and their relationships, extracted from stored memories.

### Entities
A node in the graph. Has a name, type, description, and embedding.

```rust
struct Entity {
    id: EntityId,
    name: String,          // "tokio", "api-server", "deadlock bug #42"
    entity_type: String,   // "library", "project", "event"
    description: String,   // embedded for vector search
    properties: HashMap<String, String>,
    created_at: DateTime,
    updated_at: DateTime,
}
```

### Relations
An edge between two entities.

```rust
struct Relation {
    source: EntityId,
    target: EntityId,
    predicate: String,    // "uses", "caused_by", "part_of", "depends_on"
    weight: f32,          // strength/confidence
    source_memory: MemoryId, // which memory established this relation
}
```

### Graph Operations
- **Entity extraction**: when memories are stored with `entities` param, look up or create entity nodes
- **Relation extraction**: explicit via `relate` tool or `relations` param on store. Supports causal predicates (`caused_by`, `enables`, `prevents`) as first-class edges with boosted retrieval weight.
- **Graph traversal at query time**: find seed entities via vector search on entity descriptions, expand 1-2 hops via edges, collect connected memories
- **Community detection**: deferred to v2 (Leiden algorithm over entity clusters for global queries)

### kNN Link Graph (complementary to knowledge graph)

A second graph layer stored in SQLite `memory_links` table. Unlike the knowledge graph which captures *named relationships* between entities, kNN links capture *unnamed semantic proximity* between memories.

| Aspect | Knowledge Graph | kNN Links |
|---|---|---|
| Nodes | Entities (named concepts) | Memory records |
| Edges | Typed relations (uses, caused_by) | Similarity >= 0.7 |
| Created | Explicitly by agent | Automatically at insert time |
| Traversal | petgraph in-memory | SQLite query |
| Use case | "What relates to X?" | "What's similar to this memory?" |

Both are queried in parallel during retrieval and merged via RRF.

### Storage
`petgraph::StableGraph` in memory, backed by SQLite entity/relation tables. Loaded from SQLite on startup, written through on mutations. For the expected scale of a personal/team memory system (thousands to low tens-of-thousands of entities), this fits comfortably in memory. SQLite provides durability, debuggability (`sqlite3` CLI), and a migration path to PostgreSQL for service mode.

### Entity Resolution

Agents provide inconsistent entity names ("tokio" vs "Tokio" vs "tokio runtime"). Without resolution, the knowledge graph fragments into disconnected nodes that should be one.

On entity creation, a layered resolution pipeline runs:

1. **Normalize** — lowercase, trim whitespace, collapse separators. `"Tokio"` → `"tokio"`. Check for exact match against existing entities → merge silently.
2. **Fuzzy string match** — SequenceMatcher ratio > 0.85 against existing entity names and aliases → merge silently. Catches "postgres" ↔ "postgresql".
3. **Embedding similarity** — embed the entity name, search existing entity embeddings:
   - Cosine > 0.92 → merge silently. Catches "k8s" ↔ "kubernetes".
   - Cosine 0.80-0.92 → store both, add as alias candidates, surface in `reflect` for agent review.
4. **Below 0.80** → create as new entity.

Each entity has a canonical name + alias list stored in SQLite. All lookups check aliases first. The `reflect` tool surfaces unresolved alias candidates for the agent to confirm or dismiss.

## Embedding Strategy

### Context-Enriched Embedding

Before embedding any memory, prepend metadata context to the content. This gives the embedding model type, project, and temporal information, improving retrieval precision. Anthropic's Contextual Retrieval research reported 67% reduction in retrieval failures with this approach — and it requires no LLM, just string concatenation.

Format:
```
[{type} | {namespace} | {date}] {content}
```

Examples:
```
[episodic | api-server | 2026-04-03] user debugged a deadlock in the connection pool by switching to tokio::sync::Semaphore

[semantic | api-server | 2026-04-01] api-server uses tokio 1.38

[procedural | api-server | 2026-03-15] deploy-to-staging: 1) build release 2) push to registry 3) apply k8s manifest
```

For semantic memories, the embed text is constructed from the triple: `[semantic | {namespace} | {date}] {subject} {predicate} {object}`.

This metadata prefix is added only for embedding — the stored content remains clean. The prefix is also applied to recall queries for consistency: `[query | {namespace}] {query_text}`.

## Chunking Strategy

ferrex stores memories, not documents. Most memories are short and should never be chunked — chunking short text destroys more context than it preserves (benchmarks show 54% accuracy for fragmented chunks vs 69% for intact content).

### Per-Type Chunking Rules

**Episodic (never chunk):**
Events should be self-contained and short. If content exceeds the embedding model's context window, reject with an error: "Episodic memory too long. Break into separate events." This enforces the self-contained fact format at the system level.

**Semantic (never chunk):**
Triples (subject + predicate + object) are always short. No chunking path exists for semantic memories.

**Procedural (chunk on step boundaries when needed):**
Procedural memories are structured as steps. When content exceeds the model's context window, split on step boundaries — not token counts. Each step becomes its own embedding vector, all linked to the parent memory ID with a `step_index`.

```
on store(memory):
  embed_text = format_with_context(memory)  # prepend metadata
  
  match memory.type:
    "semantic" =>
      # Triples are always short — single embedding
      embed(embed_text) → 1 vector

    "episodic" =>
      if tokens(embed_text) > model.max_context:
        return error("Episodic memory too long. Break into separate events.")
      embed(embed_text) → 1 vector

    "procedural" =>
      if tokens(embed_text) <= model.max_context:
        embed(embed_text) → 1 vector
      else:
        # Split on step boundaries (steps are already structured)
        for (i, step) in split_steps(memory.content):
          step_text = format_with_context_step(memory, step)
          embed(step_text) → 1 vector (same memory_id, step_index=i)
```

At retrieval time: Qdrant returns the best-matching chunk. ferrex deduplicates by `memory_id` (keeps highest-scoring chunk per memory), returns the full memory content from SQLite.

### Why Not Other Chunking Strategies

| Strategy | Why not for ferrex |
|---|---|
| **Sliding window** | Memories aren't documents. Step boundaries are the natural split points for procedural memories. |
| **Semantic chunking** | Designed for long documents. On short text it over-fragments (43-token average chunks, poor accuracy). |
| **Late chunking** | Requires a long-context model for full-document embedding first. Memories are already self-contained — no cross-reference problem to solve. |
| **Propositional chunking** | Requires an LLM. Our memories are already near-atomic by design (self-contained fact format). |
| **Agentic chunking** | High computational overhead, consensus is it's not worth the cost (dropped from ACL 2025 benchmarks). |
| **RAG fusion** | Increases raw recall but gains vanish after reranking (confirmed by arXiv:2603.02153, March 2026). We already have reranking. |

## Adaptive Retrieval

Not all queries need all retrieval channels. Running all 4 channels (vector, BM25, kNN links, graph) on every query wastes resources — and GraphRAG-Bench (June 2025) showed graph retrieval is 13.4% *less accurate* than vanilla RAG on single-hop factoid queries.

Query routing classifies the query and adjusts channel weights:

| Query Signal | Strategy | Example |
|---|---|---|
| Has specific identifiers, code symbols, exact names | Weight BM25 higher | "tokio::sync::Semaphore version" |
| Asks about relationships, causality | Weight graph + kNN links higher | "what caused the connection pool deadlock?" |
| Vague, conceptual, semantic | Weight vector higher | "how do we handle auth?" |
| Has temporal markers | Enable two-phase temporal retrieval | "what changed last week?" |
| Simple lookup (high BM25 confidence) | **Skip graph entirely** | "deploy-to-staging procedure" |

Classification is rule-based (regex + heuristics), not LLM-based. Zero latency overhead.

## Retrieval Pipeline Detail

```
Query: "how did we fix the connection pool issue?"

Step 1: Embed query → [0.12, -0.34, 0.56, ...]

Step 2: Query routing
  ├── Detected: causal intent ("fix"), entity ("connection pool")
  └── Strategy: vector(1.0) + BM25(0.8) + kNN_links(1.0) + graph(1.2)

Step 3: Parallel retrieval (via tokio::join!)
  ├── Vector search (Qdrant dense) → [mem_7, mem_12, mem_3, mem_19, mem_44]
  ├── BM25 search (Qdrant sparse) → [mem_12, mem_8, mem_3, mem_27, mem_15]
  ├── kNN link expansion:
  │   ├── Take top-3 vector hits as seeds
  │   └── Expand via precomputed kNN links (SQLite memory_links table)
  │   └── [mem_12→mem_22, mem_7→mem_33, mem_3→mem_41]
  └── Graph expansion (petgraph):
      ├── Entity detection in query: ["connection pool"]
      ├── Find entity node → "connection-pool" (id: e_5)
      ├── Traverse 1-2 hops:
      │     e_5 --caused_by--> e_12 ("deadlock bug")
      │     e_5 --part_of--> e_3 ("api-server")
      │     e_5 --uses--> e_8 ("tokio::sync::Semaphore")
      └── Collect memories linked to e_5, e_12, e_3, e_8 → [mem_7, mem_12, mem_22, mem_33]

Step 4: Reciprocal Rank Fusion (k=60)
  Merge four ranked lists with channel weights from Step 2.
  Documents appearing in multiple lists get boosted.
  → [mem_12, mem_7, mem_22, mem_3, mem_8, mem_33, mem_19, ...]

Step 5: Staleness filter
  Remove memories with staleness="stale" (unless include_stale=true).
  Flag "aging" memories for annotation.
  Filter out semantic facts with t_invalid set (unless include_invalidated=true).

Step 6: Reranking (fastembed cross-encoder)
  Score top-10 candidates with cross-encoder(query, memory_content)
  Apply multiplicative boosts (not additive — keeps secondary signals proportional):
    final_score = rerank_score × recency_boost × temporal_proximity_boost
  Where:
    recency_boost = 1.0 + 0.1 × (1 - age_days/365)  // ±10% over a year
    temporal_proximity_boost = 1.0 + 0.1 × temporal_relevance  // ±10%
  → [mem_12: 0.94, mem_7: 0.91, mem_22: 0.87, mem_3: 0.72, mem_8: 0.68]

Step 7: Annotate and return top-5
  Each result includes freshness metadata (age, last_accessed, staleness level).
  If multiple active semantic facts exist for the same subject+predicate, flag as contradiction.
```

### kNN Link Expansion (adopted from Hindsight)

At **insert time**, compute cosine similarity between the new memory's embedding and all existing memories. Store the top-5 neighbors with similarity >= 0.7 in the `memory_links` table (SQLite). Links are bidirectional.

At **query time**, take the top seed results from vector search and expand through their precomputed links. This provides graph-like traversal without the entity extraction overhead — memories that are semantically related are already connected.

The kNN links complement the knowledge graph: the graph captures *named relationships* (entity A "uses" entity B), while kNN links capture *unnamed semantic proximity* (these two memories are about similar things even if they don't share named entities).

### Two-Phase Temporal Retrieval (adopted from Hindsight)

When the query contains temporal markers (detected via rule-based date parsing):

1. **Phase 1 (cheap)**: Query SQLite for memories within the time window, ranked by date proximity. Return top-50 candidates.
2. **Phase 2 (expensive)**: Compute vector similarity only for those 50 candidates against the query embedding.

This avoids running expensive vector comparisons against the entire corpus when the user clearly wants time-scoped results.

## Conflict Resolution

When `store` (type: semantic) is called and an existing fact shares the same (subject, predicate):

```
Existing: ("api-server", "uses", "tokio 1.36", confidence: 0.9, t_valid: 2026-01-15, t_invalid: null)
Incoming: ("api-server", "uses", "tokio 1.38", confidence: 0.95, t_valid: 2026-04-01)
```

Resolution:
1. Compute similarity between "tokio 1.36" and "tokio 1.38" embeddings
2. Similarity < 0.95 → these are different values (not duplicates)
3. **Invalidate** old fact: set `t_invalid = 2026-04-01` (the incoming fact's `t_valid`)
4. **Insert** new fact with `t_valid = 2026-04-01`, `t_invalid = null`
5. Log the transition for auditability

The old fact is NOT deleted — it remains queryable for historical queries ("what did we use before tokio 1.38?") via `include_invalidated=true` on recall.

Edge cases:
- **Same confidence, different dates**: prefer more recent
- **Ambiguous** (e.g., subject has multiple valid values for a predicate): store both, tag as multi-valued
- **Explicit supersede**: if the agent calls `store` with a `supersedes` param pointing to an existing memory ID, skip similarity check and invalidate directly
- **Duplicate detection**: similarity >= 0.95 → deduplicate (keep higher confidence, bump `last_validated`)

## Staleness Safeguards

Stale memory is the silent killer of RAG systems. mem0's biggest failure mode is silently returning outdated facts. ferrex treats staleness as a first-class concern with multiple defense layers.

### Layer 1: Temporal Validity (semantic facts)

Every semantic fact has `t_valid` and `t_invalid` timestamps. When a fact is superseded via conflict resolution, the old fact gets `t_invalid` set — it's never silently returned as current. Queries default to `t_invalid IS NULL` (current facts only).

### Layer 2: Staleness Scoring

Every memory has a `staleness` level computed from multiple signals:

```
staleness_score = f(age, last_accessed, last_validated, access_count, type)
```

| Signal | Weight | Description |
|---|---|---|
| `age` | High | Days since creation or last update |
| `last_accessed` | Medium | Days since last retrieval (accessed memories stay fresh longer) |
| `last_validated` | High | Days since an agent implicitly or explicitly confirmed this fact |
| `access_count` | Low | Total retrievals (frequently used facts are more likely current) |

Staleness levels:
- **fresh**: within expected lifetime, recently validated
- **aging**: approaching staleness threshold, still returned but annotated
- **stale**: exceeded threshold, excluded from results by default

Thresholds (configurable per memory type):
- Episodic: fresh < 30d, aging < 90d, stale >= 90d
- Semantic: fresh < 90d since last validation, aging < 180d, stale >= 180d
- Procedural: fresh < 180d, aging < 365d, stale >= 365d

### Layer 3: Access-Time Validation Refresh

When a memory is retrieved via `recall` and the agent *uses* it (doesn't call `forget` on it), the `last_validated` timestamp is bumped. This creates a natural feedback loop: memories that keep being useful stay fresh; memories that are never retrieved drift toward staleness.

### Layer 4: Contradiction Detection at Query Time

When `recall` returns results, the system checks if multiple active semantic facts exist for the same (subject, predicate) pair with different objects. If so, both are returned with a `contradiction: true` flag and the agent can resolve it.

### Layer 5: Staleness Audit via `reflect`

The `reflect` tool (in addition to episodic consolidation) surfaces:
- Semantic facts that haven't been validated in > N days
- Facts with decaying confidence scores
- Entity nodes with no recent memory references

The agent can then confirm, update, or forget flagged memories.

### Layer 6: Result Annotation

Every recall result includes freshness metadata. The agent always knows how old and how validated a memory is. This prevents the "silently return stale data" failure mode — even if a stale memory sneaks through, the agent sees `"staleness": "aging"` and can judge accordingly.

## Memory Lifecycle

### Decay
Every memory has a `relevance_score` computed from:
- **Recency**: exponential decay from creation/last-update time (configurable half-life per type)
- **Access frequency**: memories retrieved more often decay slower
- **Validation recency**: recently validated memories decay slower
- **Explicit boost**: agent can pin important memories

Defaults:
- Episodic: half-life 30 days
- Semantic: no time-based decay, but staleness scoring based on last-validated timestamp
- Procedural: no decay

### Compaction
Periodic (or on-demand via `reflect`):
- Cluster similar episodic memories
- If N episodic memories share overlapping entities/topics → propose a semantic summary
- Agent confirms or discards proposed consolidations

### Eviction

Three fates for stale memory, depending on type:

**Episodic (evict aggressively)**:
Once an episodic memory reaches `stale` status (>90 days, rarely accessed), it becomes an eviction candidate. Before deleting, check if it is referenced as `source` by any semantic fact — if so, the episodic memory has already been distilled into durable knowledge and can safely go. Unreferenced stale episodic memories are evicted first when storage budget is exceeded.

**Semantic (never auto-evict active facts)**:
A semantic fact with `t_invalid = null` (still current) is **never auto-evicted**, even if its staleness score is high. "Stale" for an active semantic fact means "unvalidated for a while" — it might still be true. The `reflect` tool surfaces these for the agent to confirm, update, or explicitly invalidate.

Semantic facts with `t_invalid` set (superseded) are evicted after a configurable retention window (default: 180 days after invalidation). They serve historical queries ("what did we use before?") but don't need to live forever.

**Procedural (never auto-evict)**:
Procedures may be rarely used but critical when needed. Only explicitly deleted via `forget`.

**Eviction priority** (when storage exceeds budget):
1. Superseded semantic facts past retention window
2. Stale episodic memories (unreferenced by semantic facts first)
3. Stale episodic memories (referenced — source field preserved in semantic fact)
4. Aging episodic memories with lowest relevance_score
5. Never: active semantic facts, procedural memories

## Crate Structure

```
ferrex/
├── crates/
│   ├── ferrex-server/       # MCP server binary
│   │   ├── main.rs          # entry point, transport setup, Qdrant sidecar lifecycle
│   │   └── tools.rs         # MCP tool definitions (rmcp #[tool] macros)
│   │
│   ├── ferrex-core/         # memory system logic
│   │   ├── memory.rs        # memory types, store/recall/forget
│   │   ├── retrieval.rs     # hybrid retrieval pipeline, RRF, reranking
│   │   ├── conflict.rs      # contradiction detection and temporal validity
│   │   ├── lifecycle.rs     # decay, staleness scoring, compaction, eviction
│   │   ├── staleness.rs     # staleness safeguards, validation tracking
│   │   └── router.rs        # adaptive query classification and routing
│   │
│   ├── ferrex-graph/        # knowledge graph + kNN links
│   │   ├── graph.rs         # petgraph wrapper, entity/relation types
│   │   ├── traversal.rs     # graph expansion for retrieval
│   │   ├── knn_links.rs     # precomputed kNN link management
│   │   └── persistence.rs   # SQLite ↔ petgraph sync
│   │
│   ├── ferrex-embed/        # embedding engine
│   │   ├── embed.rs         # fastembed wrapper
│   │   └── rerank.rs        # cross-encoder reranking with multiplicative boosts
│   │
│   └── ferrex-store/        # storage backends
│       ├── qdrant.rs        # Qdrant client (sidecar gRPC or remote URL)
│       ├── sidecar.rs       # Qdrant sidecar process management
│       ├── db.rs            # SQLite: graph tables, kNN links, metadata, staleness
│       └── schema.rs        # SQLite migrations and table definitions
│
├── Cargo.toml
└── flake.nix
```

## Tooling Decisions

### Component Selection

| Component | Choice | Why | Service scaling path |
|---|---|---|---|
| **MCP SDK** | `rmcp` | Official Rust SDK, `#[tool]` macros, stdio transport | Add SSE transport |
| **Vector + BM25** | Qdrant (sidecar → remote) | Built-in hybrid search (dense + sparse + BM25 since v1.15), rich payload filtering, one write path for both indexes. Sidecar for local, remote URL for service mode. | Already a service — replication, sharding, multi-client |
| **kNN link cache** | SQLite `memory_links` table | Precomputed at insert time (top-5 neighbors, similarity >= 0.7). Provides graph-like expansion without entity extraction overhead. | Migrates with graph to Postgres |
| **Graph storage** | SQLite (`rusqlite`) | Debuggable (`sqlite3` CLI), FTS5 for free, SQL migration path to PostgreSQL. Entity/relation tables, temporal validity tracking, staleness metadata. | Same schema moves to Postgres trivially |
| **Graph traversal** | `petgraph` (in-memory cache) | Sub-microsecond reads during retrieval. Loaded from SQLite on startup, updated on writes. | Extract to separate graph service if needed |
| **Metadata** | Same SQLite instance | One file, one connection pool. Timestamps, access counts, staleness scores, validation timestamps, memory types — all tables alongside graph. | Migrates with graph to Postgres |
| **Embedding** | `fastembed` | 44 embedding models + 6 reranker models, ONNX quantized, local, no API keys. Maintained by Qdrant team. | Wrap in gRPC service if needed |
| **Entity extraction** | Caller provides via tool params (v1) | MCP clients (Claude) already know the entities. Zero latency, clean API contract. | Add optional NER model in v2 |

### Embedding Model Tiers

All models run locally via fastembed ONNX runtime. Configurable at startup via CLI flag or config file.

| Tier | Model | Dimensions | Size | MTEB | Context | License | fastembed enum |
|---|---|---|---|---|---|---|---|
| **small** | `all-MiniLM-L6-v2` | 384 | 90MB | 56.3 | 256 tok | Apache-2.0 | `AllMiniLML6V2` / `AllMiniLML6V2Q` |
| **mid** | `BGE-small-en-v1.5` | 384 | 67MB | ~59 | 512 tok | MIT | `BGESmallENV15` / `BGESmallENV15Q` |
| **best** | `BGE-base-en-v1.5` | 768 | 210MB | ~63 | 512 tok | MIT | `BGEBaseENV15` / `BGEBaseENV15Q` |

Extended options for specific needs:

| Tier | Model | Dimensions | Size | Notes | fastembed enum |
|---|---|---|---|---|---|
| **best-large** | `BGE-large-en-v1.5` | 1024 | 1.2GB | Highest accuracy in BGE family | `BGELargeENV15` / `BGELargeENV15Q` |
| **multilingual** | `BGE-M3` | 1024 | ~1.5GB | 100+ languages, 8192 tok context. Outputs dense + sparse + ColBERT from a single model — could unify the hybrid pipeline. | `BGEM3` |

Quantized variants (suffix `Q`) are ~50% smaller with ~2% accuracy loss.

**Default: `BGE-base-en-v1.5`** (best tier). 210MB is acceptable for a local tool, 768 dimensions gives significantly better retrieval than 384d models.

**2025-2026 contenders** (investigate fastembed-rs support during implementation):

| Model | Params | Notes |
|---|---|---|
| `Qwen3-Embedding-0.6B` | 0.6B | New MTEB leader at small size. Apache-2.0. fastembed-rs has behind feature flag. |
| `EmbeddingGemma-300M` | 300M | Google, 100+ languages, on-device optimized. |
| `nomic-embed-text` | 137M | Ultra-lightweight, good for resource-constrained setups. |

### Reranker Model Tiers

Cross-encoder rerankers re-score retrieval candidates for final ranking. Always enabled. Configurable at startup. Reranking uses **multiplicative** recency/temporal boosts (not additive) to keep secondary signals proportional to primary relevance.

| Tier | Model | Size | BEIR nDCG@10 | License |
|---|---|---|---|---|
| **small** | `ms-marco-MiniLM-L-6-v2` | 80MB | ~38 | Apache-2.0 |
| **mid** | `ms-marco-MiniLM-L-12-v2` | 120MB | ~40 | Apache-2.0 |
| **best** | `jina-reranker-v3` | 0.6B | 61.94 | — |

**2025-2026 findings**: Jina Reranker v3 (0.6B, Sept 2025) is the new winner — best BEIR score at smallest size among modern rerankers. Uses "last but not late interaction" architecture on a Qwen3 backbone. Significantly outperforms bge-reranker-base (previously our "best" tier). **Check fastembed-rs support; if unavailable, use `ort` crate directly with the ONNX model from HuggingFace.**

Other notable rerankers:
- `mxbai-rerank-large-v2` (1.5B, BEIR 61.44) — close second
- `bge-reranker-v2-m3` (0.6B, BEIR 56.51) — multilingual option
- `Qwen3-Reranker-4B` (4B, BEIR 61.16) — too large for local default

### Key Dependencies

```toml
[workspace.dependencies]
# MCP
rmcp = { version = "0.16", features = ["server", "transport-io", "macros"] }

# Async runtime
tokio = { version = "1", features = ["full"] }

# Embedding + reranking
fastembed = "5"

# Vector + BM25 store
qdrant-client = "1"

# Knowledge graph (in-memory)
petgraph = { version = "0.7", features = ["serde-1"] }

# Graph + metadata persistence
rusqlite = { version = "0.32", features = ["bundled"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# MCP tool schemas
schemars = "1"

# Error handling
anyhow = "1"
thiserror = "2"

# Logging (stderr only — stdout is MCP JSON-RPC protocol)
tracing = "0.1"
tracing-subscriber = "0.3"

# Time
chrono = { version = "0.4", features = ["serde"] }

# IDs
uuid = { version = "1", features = ["v7", "serde"] }
```

## Open Questions

1. **Reranker model selection**: Jina Reranker v3 is the quality leader but may not be in fastembed-rs yet. Check availability; if not, benchmark `ms-marco-MiniLM-L-12-v2` (fastembed built-in) vs loading Jina v3 via `ort` directly.

2. **fastembed ONNX Runtime in Nix**: needs `onnxruntime` in `flake.nix` buildInputs. May require system library or static linking. Needs investigation for the Nix build.

3. **Reflect tool scope**: how much intelligence should `reflect` have? Minimal: cluster episodic memories by similarity, surface staleness audit, flag contradictions. Maximal: extract entity-relation triples and propose semantic facts. The minimal version works without any LLM; the maximal version needs one.

4. **Qdrant sidecar packaging**: how to distribute the Qdrant binary alongside ferrex? Options: (a) expect user to install Qdrant separately, (b) download on first run, (c) bundle in Nix flake. Nix makes (c) straightforward.

5. **BGE-M3 as unified model**: BGE-M3 outputs dense + sparse + ColBERT from a single forward pass. This could replace the separate BM25 index entirely (Qdrant's sparse vector support can ingest the sparse output directly). Worth benchmarking vs separate BGE-base + Qdrant BM25 tokenization.

6. **kNN link maintenance at scale**: with 10k+ memories, computing top-5 neighbors at insert time requires scanning all existing embeddings. At small scale this is fine (Qdrant search handles it). At larger scale, may need to limit search to same-type or same-project memories.

## Non-Goals (v1)

- Multi-user / multi-tenant support
- Cloud sync or remote storage
- Community detection / Leiden algorithm (v2)
- Automatic entity extraction from free text (require explicit entities in v1)
- Multimodal embeddings (text only in v1)
- SSE transport (stdio only in v1)
- LLM-based memory extraction (mem0's approach — intentionally avoided due to detail loss)

## Implementation Phases

### Phase 1: Foundation (~900 LOC)
- Scaffold workspace and crates
- Qdrant sidecar lifecycle management (start/stop/health check)
- fastembed wrapper (embed + rerank, configurable model tiers)
- Qdrant client (vector + BM25 write/search via gRPC)
- SQLite schema (entities, relations, memory_links, metadata tables, temporal validity columns)
- Basic `store` (episodic only) and `recall` (vector-only) MCP tools
- stdio transport via rmcp

### Phase 2: Hybrid Retrieval + kNN Links (~700 LOC)
- BM25 via Qdrant built-in sparse index
- kNN link computation at insert time (top-5 neighbors, similarity >= 0.7)
- kNN link expansion in retrieval
- Reciprocal Rank Fusion (k=60) with channel weights
- Cross-encoder reranking with multiplicative recency boosts
- Adaptive query routing (rule-based classifier)
- Two-phase temporal retrieval

### Phase 3: Knowledge Graph + Conflict Resolution (~700 LOC)
- petgraph entity/relation model (including causal predicates)
- SQLite persistence (entity/relation tables ↔ petgraph sync)
- `store` semantic type support, `relate` tool
- Graph expansion in retrieval pipeline (seed + traverse + collect)
- Conflict detection with temporal validity (`t_valid`/`t_invalid`)
- Contradiction detection at query time

### Phase 4: Memory Lifecycle + Staleness (~600 LOC)
- `store` procedural type support, `forget`, `stats` tools
- Staleness scoring (age + access + validation recency)
- Staleness levels (fresh/aging/stale) with configurable thresholds
- Access-time validation refresh
- Freshness metadata annotation on recall results
- Decay scoring, deduplication on write, eviction policy
- `reflect` tool (cluster + surface + staleness audit + contradiction alerts)

### Phase 5: Polish (~400 LOC)
- Semantic caching on recall (LRU, embed-hash keyed)
- Parallel retrieval (vector + BM25 + kNN + graph concurrently via tokio::join!)
- Nix build with ONNX runtime + Qdrant binary
- Integration tests
- MCP Inspector validation
