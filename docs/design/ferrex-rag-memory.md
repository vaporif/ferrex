# ferrex: RAG Memory MCP Server

## Overview

ferrex is a local-first MCP server that provides intelligent long-term memory for AI agents. It combines vector search and BM25 keyword matching into a hybrid retrieval system — exposed through memory-typed tools that agents interact with naturally.

The goal is not another vector store with an MCP wrapper. It is a memory system that understands temporal facts, resolves contradictions, manages its own lifecycle, and retrieves context through complementary signal paths fused into one result.

## Design Principles

1. **Minimal ops** — one Rust binary + Qdrant sidecar. No Python, no Docker compose stacks, no cloud accounts.
2. **Memory semantics, not storage semantics** — the API speaks in episodic/semantic/procedural terms, not vectors and indexes.
3. **Retrieval quality over storage volume** — hybrid search with reranking by default. Better to return 3 excellent results than 10 mediocre ones.
4. **Temporal awareness** — every fact has a validity timeline. Contradictions are detected and resolved, not silently accumulated.
5. **Staleness-aware** — stale memories are detected, flagged, and decayed. The system never silently returns outdated facts without signaling freshness.
6. **Local-first** — embeddings generated locally (ONNX), data stored locally, no network calls required for core operation.

## Competitive Positioning

The landscape (as of April 2026):

### Retrieval-Based Memory Systems

| System | Approach | ferrex differentiator |
|---|---|---|
| **mem0** | LLM-based extraction → vector store | No LLM dependency; preserves original detail |
| **Hindsight** | 4-network retrieval, Python/FastAPI, Postgres. SOTA on BEAM 10M (64.1%) | Single Rust binary, no Python runtime |
| **Honcho** | Continual learning with custom reasoning models (Neuromancer). LLM-heavy, cloud-first. 90.4% LongMemEval-S, 40.6% BEAM 10M | LLM-free, local-first. ferrex keeps reasoning in the calling agent, not the server |
| **Cognee** | KG + vector, Python, 30+ connectors | Lightweight, embeddable, MCP-native |
| **Zep/Graphiti** | Temporal KG, Neo4j. 71.2% LongMemEval-S | No Neo4j; SQLite + Qdrant sidecar |
| **Letta/MemGPT** | Self-editing memory, agent controls recall | Hybrid search (vector + BM25), temporal validity |
| **Supermemory** | MCP-native, ~85% LongMemEval-S (gpt-4o) | Local-first, no cloud dependency, temporal validity |
| **A-Mem** | Zettelkasten-style linked notes (NeurIPS 2025) | Hybrid search, temporal awareness, staleness safeguards |
| **Memobase.ai** | MCP-native memory-as-a-service | Local-first, no cloud dependency |

### Compaction-Based Memory (Different Problem Class)

| System | Approach | Why ferrex is complementary, not competing |
|---|---|---|
| **Mastra OM** | Two background LLM agents compress conversation into dense observation log. No retrieval. 84.2% LongMemEval-S (gpt-4o), 94.9% (gpt-5-mini) | OM manages intra-session context windows; ferrex provides persistent cross-session queryable memory. OM can't recall facts from a different project or last month — it compresses what's in the current window. The two are complementary |

### Rust MCP Memory Servers

| System | Approach | What ferrex adds |
|---|---|---|
| **memory-mcp-rs** | Rust + SQLite KG, FTS5 search | Vector search, BM25, reranking, temporal validity, memory types |
| **memory-mcp** | Markdown files in git + fastembed | Hybrid search, entity resolution, staleness detection, conflict resolution |
| **rusty-mcp** | Qdrant + Ollama embeddings | No Ollama dependency, BM25, memory type semantics, temporal awareness |
| **sqlite-mcp-rs** | SQLite + optional vector/fastembed | Memory-typed API, entity resolution, lifecycle management |

These are thin wrappers — vector store or KG with MCP glue. None combine memory type semantics, temporal validity, staleness detection, entity resolution, conflict detection, hybrid search with server-side RRF, and cross-encoder reranking.

### Benchmark Landscape

LongMemEval-S (~115k tokens) is approaching obsolescence as context windows grow beyond 128k. BEAM at 10M tokens is the emerging stress test where context stuffing is impossible and only real memory architectures survive. Current BEAM 10M leaders: Hindsight (64.1%), Honcho (40.6%).

ferrex's position: **Rust-native + Qdrant (sidecar or external) + hybrid search (vector + BM25) + temporal validity + staleness safeguards + LLM-free + MCP-native**. No published benchmark scores yet — BEAM 10M and LongMemEval evaluation planned post-v1.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    MCP Transport                     │
│              (rmcp, stdio)                            │
└────────────────────────┬────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────┐
│                    Tool Router                       │
│       store / recall / forget / reflect / stats      │
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
│  │  • Entity/relation tables     │                    │
│  │  • Temporal validity tracking │                    │
│  │  • Metadata, access counts    │                    │
│  │  • Staleness scores           │                    │
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

### Qdrant Connection

ferrex communicates with Qdrant via gRPC using `qdrant-client`. Two modes:

1. **Sidecar (default)** — ferrex manages a local Qdrant subprocess. Starts on launch, stops on shutdown. Invisible to the user. Data stored at `~/.ferrex/qdrant-data/`.
2. **External (`--qdrant-url <url>`)** — ferrex connects to a user-provided Qdrant instance. No sidecar process, no local data directory. Use this for team deployments, existing infrastructure, or when you prefer to manage Qdrant yourself. Fail-fast: one connection attempt with 3-second timeout. On failure: `"error: cannot connect to Qdrant at {url} — is it running?"` and exit. No retry — the user manages the instance.

When `--qdrant-url` is set, sidecar management is skipped entirely. The same `qdrant-client` code handles both modes — the only difference is the connection target.

This trades pure single-binary for access to Qdrant's full feature set: HNSW with fused payload filtering, sparse vectors for BM25 (server-side tokenization + IDF), named vectors for hybrid search, and the Query API for server-side RRF fusion in a single request.

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
  "entities": ["connection-pool", "tokio::sync::Semaphore", "api-server"]
}
```

- **Storage**: vector embedding + BM25 index + metadata
- **Retrieval**: temporal + similarity search
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

- **Storage**: vector embedding + BM25 index + entity metadata + SQLite tables
- **Retrieval**: exact match + semantic search + entity filtering
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

Tool count is kept low (5 tools) to minimize context window tax. Research shows MCP tools can consume 16%+ of context — fewer, richer tools are better.

### Tool Descriptions as Agent Instructions

MCP tool descriptions are loaded into the agent's context at session start. They are the primary mechanism for guiding agent behavior — no hooks, no system prompt injection needed. The descriptions below are carefully crafted to serve as both API documentation *and* implicit behavioral instructions.

### `store`

**MCP description** (what the agent sees):
> Store a memory for long-term recall. Call this whenever you learn something worth remembering: new facts about the user or project, decisions made, problems solved, workflows discovered, or corrections to previous knowledge. You can specify type explicitly ("episodic" for events, "semantic" for stable facts, "procedural" for workflows) or omit it and the system will auto-detect from the fields you provide. Write self-contained memories — each should make sense on its own without surrounding context. Include relevant entities for filtering. If this updates a previously known fact, the system detects and resolves the contradiction automatically. Near-duplicate memories are rejected automatically.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `type` | string | no | `"episodic"`, `"semantic"`, or `"procedural"`. Auto-detected when omitted: semantic if `subject`+`predicate`+`object` provided, procedural if `steps`/`conditions` provided, episodic otherwise. |
| `content` | string | yes* | What happened (episodic) or the procedure steps (procedural). Self-contained format recommended: "what \| when \| where \| who \| why" |
| `subject` | string | yes* | The entity this fact is about (semantic only) |
| `predicate` | string | yes* | The relationship or property (semantic only) |
| `object` | string | yes* | The value or target entity (semantic only) |
| `confidence` | float | no | 0.0-1.0, defaults to 1.0 |
| `source` | string | no | Provenance (memory ID, URL, etc.) |
| `entities` | string[] | no | Entity names for filtering and future knowledge graph expansion |
| `context` | object | no | Structured context (task, project, outcome, etc.) |
| `supersedes` | string | no | Memory ID to explicitly replace (skips similarity check) |

*Required fields depend on `type` (explicit or auto-detected): episodic/procedural require `content`; semantic requires `subject`+`predicate`+`object`.

**On store, the ingestion pipeline**:
1. **Type resolution**: if `type` omitted, auto-detect from provided fields
2. **Deduplication check**: search existing same-type memories by embedding similarity. If cosine > 0.95 → reject with `"similar memory already exists: {id}"`. Prevents agents from storing the same fact repeatedly with slight rewording.
3. **Chunking** (if needed): apply type-aware chunking (see Chunking Strategy)
4. Embed via fastembed → write dense vector to Qdrant. BM25 sparse vectors are computed server-side by Qdrant (send raw text, Qdrant handles tokenization + IDF via `Modifier::IDF` on `SparseVectorParams`)
5. **Entity resolution**: resolve entity names via layered pipeline (normalize → fuzzy → embedding → alias), store as Qdrant payload metadata + SQLite entity table (see Entity Resolution)
6. Write metadata to SQLite (timestamps, access counts, staleness fields)
7. For semantic type: run conflict detection (see Conflict Resolution)
8. For procedural type: create new version if name already exists

### `recall`

**MCP description** (what the agent sees):
> Search long-term memory. Call this whenever you need to remember something: past discussions, known facts, established workflows, or entity relationships. Use `stats` at the start of a conversation for a quick overview — use `recall` when you have a specific question. Returns results ranked by relevance with freshness metadata — check the staleness field to gauge how current each memory is.

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
2. Single Qdrant Query API call: prefetch dense + sparse, fuse via server-side RRF (k=60)
3. Cross-encoder reranking (top-20 candidates) with **multiplicative** recency boost
4. Return top-N with scores and metadata
5. (Phase 4 adds: staleness filter, staleness annotation, freshness metadata)

**Each result includes metadata (Phase 2: basic, Phase 4: full freshness):**

Phase 2 response shape:
```json
{
  "id": "mem_12",
  "content": "...",
  "score": 0.94,
  "age_days": 45
}
```

Phase 4 adds freshness metadata:
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

`staleness` field values (Phase 4): `"fresh"`, `"aging"` (approaching staleness threshold), `"stale"` (exceeded threshold, returned only if `include_stale=true`), `"superseded"` (a newer fact exists for the same subject+predicate).

### `forget`

**MCP description** (what the agent sees):
> Delete or invalidate memories that are no longer accurate or relevant. Use this when you discover a memory is wrong, outdated, or the user asks you to forget something. First use recall to find the memory IDs, then pass them here.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `ids` | string[] | yes | Specific memory IDs to delete |
| `cascade` | bool | no | Also remove entity-memory links for entities only referenced by forgotten memories |

### `reflect`

**MCP description** (what the agent sees):
> Audit memory health. Call this periodically (e.g., end of a long session or weekly) to: surface stale memories that need review, detect contradictions between active facts, and identify memories that haven't been validated recently. Review the results and use store/forget to address issues.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `scope` | string | no | Limit reflection to a namespace/topic |
| `window` | string | no | Time window to reflect over, default "7d" |

Returns:
- List of stale/unvalidated memories that need review
- Contradiction alerts (multiple active facts for same subject+predicate)
- Memories with lowest access counts (candidates for forget)

### `stats`

**MCP description** (what the agent sees):
> Memory system overview. Call this at the START of every conversation for a quick status, or with `detail=true` for full diagnostics. Default (brief) mode returns just what needs attention and recent context — enough to orient without wasting tokens. Detailed mode adds counts, staleness distribution, and storage info.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `detail` | bool | no | Default `false` (brief mode). Set `true` for full diagnostics. |

**Brief mode (default)** — optimized for conversation-start. Minimal token cost:

```json
{
  "total": 241,
  "recent": [
    { "id": "mem_241", "type": "episodic", "summary": "debugged connection pool deadlock", "age_days": 0 },
    { "id": "mem_240", "type": "semantic", "summary": "api-server uses tokio 1.38", "age_days": 2 }
  ],
  "needs_attention": {
    "stale_count": 12,
    "conflict_count": 3,
    "unvalidated_count": 8
  }
}
```

**Detailed mode** (`detail=true`) — full diagnostics for health checks:

```json
{
  "counts": { "episodic": 142, "semantic": 87, "procedural": 12 },
  "staleness": { "fresh": 198, "aging": 31, "stale": 12 },
  "conflicts": 3,
  "entities": 64,
  "storage_mb": 42.1,
  "recent": [
    { "id": "mem_241", "type": "episodic", "summary": "debugged connection pool deadlock", "age_days": 0 },
    { "id": "mem_240", "type": "semantic", "summary": "api-server uses tokio 1.38", "age_days": 2 }
  ],
  "needs_attention": {
    "stale_count": 12,
    "conflict_count": 3,
    "unvalidated_count": 8
  }
}
```

## Entity Storage

Entities mentioned in memories are stored in SQLite and as Qdrant payload metadata for filtering. This provides the foundation for future knowledge graph expansion (see `future-improvements.md`).

### Entities

```rust
struct Entity {
    id: EntityId,
    name: String,          // "tokio", "api-server", "deadlock bug #42"
    entity_type: String,   // "library", "project", "event"
    created_at: DateTime,
    updated_at: DateTime,
}
```

### Entity Operations
- **Entity extraction**: when memories are stored with `entities` param, look up or create entity rows in SQLite
- **Payload filtering**: entities stored as Qdrant payload on each memory point, enabling filtered vector search (`recall` with `entities` param)
- **Memory-entity links**: SQLite junction table tracks which memories mention which entities

### Entity Resolution

Agents provide inconsistent entity names ("tokio" vs "Tokio" vs "tokio runtime"). Without resolution, filtering fragments across variant names.

Layered resolution pipeline:
1. **Normalize** — lowercase, trim whitespace, collapse separators. `"Tokio"` → `"tokio"`. Check for exact match against existing entities → merge silently.
2. **Fuzzy match** — SequenceMatcher ratio > 0.85 against existing entity names and aliases → merge. Catches "postgres" ↔ "postgresql".
3. **Embedding similarity** — cosine > 0.92 → merge. Catches semantically equivalent but lexically different names.
4. **Ambiguous** — embedding similarity 0.80-0.92 → store both, add as alias candidates, surface in `reflect` for agent review.
5. **No match** → create as new entity.

Each entity has a canonical name + list of aliases. All lookups check aliases first.

## Embedding Strategy

### Plain-Text Embedding + Payload Filtering

Memories are embedded as **plain text only** — no metadata prefixes. Type, namespace, and date are stored as Qdrant payload fields and filtered at query time using Qdrant's payload filtering.

**Why not metadata prefixes?** BGE-base-en-v1.5 expects plain text on the document side (it was trained with a specific query instruction prefix, not arbitrary metadata). Structured prefixes like `[type | namespace | date]` are out-of-distribution and likely degrade embedding quality. Anthropic's Contextual Retrieval (which reported 67% improvement) uses LLM-generated natural language prose, not structured metadata concatenation — a fundamentally different technique.

**Embed text by type:**
- Episodic: embed the `content` field directly
- Semantic: embed `{subject} {predicate} {object}` as a natural sentence
- Procedural: embed the `content` field directly

**Query-time filtering** via Qdrant payload:
- `type` filter: restrict to specific memory types
- `namespace` filter: scope to project/workspace
- `entities` filter: restrict to memories mentioning specific entities
- Temporal filters: `t_valid`/`t_invalid` for semantic facts, timestamp ranges for episodic

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
  embed_text = match memory.type:
    "semantic" => "{subject} {predicate} {object}"
    "episodic" | "procedural" => memory.content
  
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
          embed(step) → 1 vector (same memory_id, step_index=i)
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

## Retrieval Pipeline Detail

v1 uses two retrieval channels (vector + BM25) fused via RRF, followed by cross-encoder reranking. Adaptive query routing and additional channels (kNN links, graph expansion) are deferred to v2 pending retrieval quality measurements (see `future-improvements.md`).

```
Query: "how did we fix the connection pool issue?"

Step 1: Embed query → [0.12, -0.34, 0.56, ...]

Step 2: Hybrid retrieval via Qdrant Query API (single request)
  prefetch: [
    { query: dense_vector, using: "dense", limit: 30 },
    { query: sparse_vector, using: "sparse", limit: 30 }
  ]
  query: Fusion::RRF (k=60, server-side)
  → [mem_12, mem_7, mem_3, mem_8, mem_19, ...]

  Qdrant fuses dense + BM25 results server-side using RRF in one round-trip.
  No client-side fusion code needed. BM25 sparse vectors are also computed
  server-side (Qdrant handles tokenization + IDF since v1.15).

Step 3: Reranking (fastembed cross-encoder)
  Score top-20 candidates with cross-encoder(query, memory_content)
  Apply multiplicative recency boost (not additive — keeps secondary signal proportional):
    final_score = rerank_score × recency_boost
  Where:
    recency_boost (type-specific, half-life decay, floor at 1.0):
      episodic:   1.0 + 0.3 × 2^(-age_days/30)    // half-life 30d, range 1.0-1.3
      semantic:   1.0 + 0.15 × 2^(-age_days/180)   // half-life 180d, range 1.0-1.15
      procedural: 1.0                                // no boost
  → [mem_12: 0.94, mem_7: 0.91, mem_3: 0.87, mem_8: 0.72, mem_19: 0.68]

Step 4: Return top-5
  Results include memory content, scores, and basic metadata.

  Note: Staleness filtering and freshness annotations are added in Phase 4
  when the staleness scoring machinery exists. Phase 2 returns all matched
  results without staleness metadata.
```

## Conflict Resolution

When `store` (type: semantic) is called, conflict detection looks for existing facts that match on (subject, predicate) — with predicate normalization to catch semantic equivalents.

### Predicate Normalization

Agents use inconsistent predicates ("uses" vs "depends-on" vs "requires"). Without normalization, conflict detection misses obvious contradictions.

Resolution pipeline (applied before conflict matching):
1. **Normalize** — lowercase, trim, collapse separators, strip hyphens/underscores. `"depends-on"` → `"dependson"`.
2. **Synonym map** — static canonical mapping for common predicate families:
   - `uses`, `depends_on`, `requires`, `needs` → canonical `depends_on`
   - `written_in`, `implemented_in`, `built_with` → canonical `built_with`
   - `version`, `runs_version`, `at_version` → canonical `version`
   - `owned_by`, `maintained_by`, `managed_by` → canonical `owned_by`
3. **Fuzzy match** — SequenceMatcher ratio > 0.85 against existing predicates for the same subject → treat as same predicate.

The synonym map is extensible via config. Unrecognized predicates pass through as-is — fuzzy matching catches most remaining equivalences.

### Conflict Detection

```
Existing: ("api-server", "uses", "tokio 1.36", confidence: 0.9, t_valid: 2026-01-15, t_invalid: null)
Incoming: ("api-server", "depends-on", "tokio 1.38", confidence: 0.95, t_valid: 2026-04-01)
         ↓ predicate normalization: "depends-on" → "depends_on", "uses" → "depends_on" → match!
```

Resolution:
1. Match existing facts by (resolved_subject, normalized_predicate)
2. Compute similarity between object values ("tokio 1.36" vs "tokio 1.38")
3. Similarity < 0.95 → these are different values (not duplicates)
4. **Invalidate** old fact: set `t_invalid = 2026-04-01` (the incoming fact's `t_valid`)
5. **Insert** new fact with `t_valid = 2026-04-01`, `t_invalid = null`
6. Log the transition for auditability

The old fact is NOT deleted — it remains queryable for historical queries ("what did we use before tokio 1.38?") via `include_invalidated=true` on recall.

Edge cases:
- **Same confidence, different dates**: prefer more recent
- **Ambiguous** (e.g., subject has multiple valid values for a predicate): store both, tag as multi-valued
- **Explicit supersede**: if the agent calls `store` with a `supersedes` param pointing to an existing memory ID, skip similarity check and invalidate directly
- **Duplicate detection**: similarity >= 0.95 → deduplicate (keep higher confidence, bump `last_validated`)
- **Unresolved predicates**: if predicates don't match via normalization, synonym map, or fuzzy match, treat as distinct predicates (no conflict)

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
| `last_accessed` | Low | Days since last retrieval (popularity signal, not correctness signal) |
| `last_validated` | High | Days since an agent explicitly confirmed this fact (see Layer 3) |
| `access_count` | Low | Total retrievals (frequently used facts are more likely current) |

Staleness levels:
- **fresh**: within expected lifetime, recently validated
- **aging**: approaching staleness threshold, still returned but annotated
- **stale**: exceeded threshold, excluded from results by default

Thresholds (configurable per memory type, overridable per namespace):
- Episodic: fresh < 30d, aging < 90d, stale >= 90d
- Semantic: fresh < 90d since last validation, aging < 180d, stale >= 180d
- Procedural: fresh < 180d, aging < 365d, stale >= 365d

Per-namespace overrides allow projects to tune thresholds to their domain. A fast-moving CI/CD project might set procedural staleness to 30d; a reference knowledge base might set semantic staleness to 365d. Configured via `ferrex.toml` or `--staleness-config`:

```toml
[staleness.defaults]
episodic = { fresh = 30, aging = 90, stale = 90 }
semantic = { fresh = 90, aging = 180, stale = 180 }
procedural = { fresh = 180, aging = 365, stale = 365 }

[staleness.namespaces."ci-infra"]
procedural = { fresh = 7, aging = 14, stale = 30 }

[staleness.namespaces."company-reference"]
semantic = { fresh = 365, aging = 730, stale = 730 }
```

### Layer 3: Retrieval vs Validation (Separate Signals)

Retrieval and validation are distinct signals with different meanings:

- **`last_accessed`** — bumped on every `recall` hit. Used for decay scoring (frequently retrieved memories decay slower). This is a popularity signal, not a correctness signal.
- **`last_validated`** — bumped only by explicit agent actions that confirm the memory is still accurate:
  1. `store(supersedes: id)` — the agent updates a fact, implicitly confirming the subject area
  2. `reflect` confirmation — the agent reviews a stale memory and confirms it
  3. `store(source: "memory:id")` — the agent creates a new memory citing this one as source

Retrieval alone does NOT bump `last_validated`. A popular-but-wrong memory that keeps appearing in results will still age toward staleness based on its validation timestamp. This prevents the positive feedback loop where frequently-retrieved stale facts perpetually appear fresh.

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

### Compaction (v2)
Deferred to v2 (see `future-improvements.md`). Requires episodic clustering and semantic promotion via `reflect`.

### Eviction

Three fates for stale memory, depending on type:

**Episodic (evict aggressively)**:
Once an episodic memory reaches computed `stale` staleness level (based on age + access frequency + validation recency, not raw age alone), it becomes an eviction candidate. Before deleting, check if it is referenced as `source` by any semantic fact — if so, the episodic memory has already been distilled into durable knowledge and can safely go. Unreferenced stale episodic memories are evicted first when storage budget is exceeded.

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
│   │   ├── retrieval.rs     # retrieval pipeline orchestration, recency boosts, final scoring (RRF is server-side in Qdrant)
│   │   ├── conflict.rs      # contradiction detection and temporal validity
│   │   ├── lifecycle.rs     # decay, staleness scoring, compaction, eviction
│   │   └── staleness.rs     # staleness safeguards, validation tracking
│   │
│   ├── ferrex-embed/        # embedding engine
│   │   ├── embed.rs         # fastembed wrapper
│   │   └── rerank.rs        # fastembed TextRerank wrapper (raw relevance scores only)
│   │
│   └── ferrex-store/        # storage backends
│       ├── qdrant.rs        # Qdrant client (sidecar gRPC or remote URL)
│       ├── sidecar.rs       # Qdrant sidecar process management
│       ├── db.rs            # SQLite: entity tables, metadata, staleness
│       └── schema.rs        # SQLite migrations and table definitions
│
├── Cargo.toml
└── flake.nix
```

## Tooling Decisions

### Component Selection

| Component | Choice | Why | Service scaling path |
|---|---|---|---|
| **MCP SDK** | `rmcp` | Official Rust SDK, `#[tool]` macros, stdio transport | Add Streamable HTTP transport |
| **Vector + BM25** | Qdrant (sidecar or external) | Built-in hybrid search via Query API (prefetch dense + sparse, server-side RRF fusion in one request). BM25 tokenization + IDF computed server-side since v1.15. Rich payload filtering. Sidecar for local, `--qdrant-url` for external. | Already a service — replication, sharding, multi-client |
| **Metadata + entities** | SQLite (`rusqlite`) | Debuggable (`sqlite3` CLI), one file, one connection pool. Entity tables, temporal validity, staleness metadata. | Same schema moves to Postgres trivially |
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

Cross-encoder rerankers re-score retrieval candidates for final ranking. Always enabled. Configurable at startup. ferrex-embed exposes raw reranker scores only. Recency boosts and final scoring live in ferrex-core/retrieval.rs where memory type and timestamp data is available.

| Tier | Model | Size | BEIR nDCG@10 | License | fastembed enum |
|---|---|---|---|---|---|
| **default** | `BAAI/bge-reranker-base` | 278MB | ~52 | MIT | `BGERerankerBase` |
| **multilingual** | `jinaai/jina-reranker-v2-base-multilingual` | ~560MB | ~55 | — | `JINARerankerV2BaseMultilingual` |

Other fastembed built-in rerankers:
- `rozgo/bge-reranker-v2-m3` (multilingual, `BGERerankerV2M3`)
- `jinaai/jina-reranker-v1-turbo-en` (English, `JINARerankerV1TurboEn`)

**2025-2026 contenders** (not in fastembed built-ins — require `UserDefinedRerankingModel` with ONNX files from HuggingFace, or direct `ort` crate loading):

Other notable rerankers:
- `mxbai-rerank-large-v2` (1.5B, BEIR 61.44) — close second
- `bge-reranker-v2-m3` (0.6B, BEIR 56.51) — multilingual option
- `Qwen3-Reranker-4B` (4B, BEIR 61.16) — too large for local default

### Key Dependencies

```toml
[workspace.dependencies]
# MCP
rmcp = { version = "1", features = ["server", "transport-io", "macros"] }  # verify feature flags against 1.x migration guide

# Async runtime
tokio = { version = "1", features = ["full"] }

# Embedding + reranking
fastembed = "5"

# Vector + BM25 store
qdrant-client = "1"

# Metadata + entity persistence
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

1. **Reranker model selection**: fastembed built-in rerankers are `bge-reranker-base` (default), `bge-reranker-v2-m3`, `jina-reranker-v1-turbo-en`, and `jina-reranker-v2-base-multilingual`. Jina Reranker v3 and ms-marco-MiniLM are NOT built-in — they require `UserDefinedRerankingModel` with ONNX files or direct `ort` crate loading. Benchmark `bge-reranker-base` as v1 default; evaluate jina-v3 via custom loading if quality insufficient.

2. **Qdrant sidecar packaging**: how to distribute the Qdrant binary alongside ferrex? Options: (a) expect user to install Qdrant separately, (b) download on first run, (c) bundle in Nix flake. Nix makes (c) straightforward.

3. **BGE-M3 as unified model**: BGE-M3 outputs dense + sparse + ColBERT from a single forward pass. This could replace the separate BM25 index entirely (Qdrant's sparse vector support can ingest the sparse output directly). Worth benchmarking vs separate BGE-base + Qdrant server-side BM25.

## Non-Goals (v1)

- Multi-user / multi-tenant support
- Cloud sync or remote storage
- Knowledge graph traversal as retrieval channel (v2 — see `future-improvements.md`)
- kNN link graphs (v2 — see `future-improvements.md`)
- Adaptive query routing (v2 — see `future-improvements.md`)
- Community detection / Leiden algorithm (v2)
- Automatic entity extraction from free text (require explicit entities in v1)
- Multimodal embeddings (text only in v1)
- Streamable HTTP transport (stdio only in v1)
- LLM-based memory extraction (mem0's approach — intentionally avoided due to detail loss)

## Testing Strategy

Tests are written alongside each implementation phase, not deferred to the end. Each subsystem has unit tests; integration tests require a real Qdrant instance.

### Unit Tests (no external dependencies)

**Staleness scoring** (`ferrex-core/staleness.rs`):
- Compute staleness levels for each memory type at boundary conditions (29d, 30d, 31d for episodic fresh/aging)
- Verify `last_accessed` does NOT bump `last_validated`
- Verify explicit validation actions (supersede, reflect confirm, source citation) DO bump `last_validated`
- Per-namespace threshold overrides apply correctly
- Decay formula produces expected scores (half-life math)

**Conflict resolution** (`ferrex-core/conflict.rs`):
- Exact (subject, predicate) match triggers conflict detection
- Predicate normalization: "uses" and "depends-on" match via synonym map
- Predicate fuzzy matching: "depends_on" and "dependson" match at ratio > 0.85
- Unrelated predicates don't trigger false conflicts
- Object similarity stages: exact dedup (>0.95), clear different (<0.5), middle-ground fallback to embedding
- `t_invalid` set correctly on superseded facts
- `supersedes` param bypasses similarity check
- Multi-valued predicates handled (both stored, tagged)

**Entity resolution** (`ferrex-core/entity.rs`):
- Normalization: "Tokio" → "tokio", "  api  server  " → "api server"
- Fuzzy match: "postgres" ↔ "postgresql" merges above 0.85
- Alias lookup: querying by alias returns canonical entity
- Ambiguous range (0.80-0.92 embedding similarity): both stored, alias candidate created
- No match: new entity created

**Deduplication** (`ferrex-core/memory.rs`):
- Cosine > threshold rejects with existing ID
- Cosine <= threshold allows store
- `supersedes` param bypasses dedup check
- Different memory types don't cross-deduplicate

**Chunking** (`ferrex-core/chunking.rs`):
- Episodic: rejects content exceeding model context window
- Semantic: triples always produce single vector
- Procedural: splits on step boundaries when exceeding context
- Procedural: short content produces single vector
- Step chunks share memory_id with correct step_index

**Memory type auto-detection** (`ferrex-core/memory.rs`):
- subject+predicate+object → semantic
- steps or conditions → procedural
- everything else → episodic
- Explicit type overrides auto-detection

**Recency boost formulas** (`ferrex-core/retrieval.rs`):
- Episodic at age 0: boost = 1.3, at age 30d: boost = 1.15, at age 300d: boost ≈ 1.0
- Semantic at age 0: boost = 1.15, at age 180d: boost = 1.075
- Procedural: always 1.0
- Boosts are multiplicative with rerank score
- Ranges large enough to shift rankings (cross-encoder scores vary by 0.2-0.5 between candidates)

### Integration Tests (require Qdrant)

Run against a real Qdrant instance (sidecar mode — test starts/stops its own Qdrant). Use `#[ignore]` attribute for CI environments without Qdrant, or gate behind a `integration` feature flag.

**Store → Recall round-trip**:
- Store episodic memory, recall by semantic query, verify returned
- Store semantic triple, recall by subject name, verify returned
- Store procedural memory, recall by condition description, verify returned

**Hybrid retrieval quality**:
- Store 20 memories with known content. Query with terms that should match via BM25 (exact keywords) and via vector (semantic similarity). Verify both channels contribute — a result found only by BM25 and a result found only by vector search both appear in final results.

**Deduplication end-to-end**:
- Store a memory, attempt to store a near-identical rewording, verify rejection
- Store with `supersedes` param, verify original invalidated

**Conflict resolution end-to-end**:
- Store ("X", "uses", "v1"), then ("X", "depends-on", "v2"). Verify first fact gets `t_invalid` set. Recall with `include_invalidated=true`, verify both returned. Recall without, verify only v2 returned.

**Entity resolution end-to-end**:
- Store memory with entity "Tokio", store another with "tokio runtime". Recall filtering by entity "tokio" returns both.

**Staleness lifecycle**:
- Store a memory, artificially age it (set timestamps in the past). Verify staleness level transitions. Verify stale memories excluded from default recall, included with `include_stale=true`.

**Sidecar lifecycle**:
- Start ferrex, verify Qdrant sidecar starts (PID file created)
- Start second ferrex instance, verify it reuses existing sidecar
- Stop first instance (the one that started sidecar), verify sidecar stops
- Stop second instance, verify clean shutdown

**Stats brief vs detailed**:
- Store a mix of memory types. Call `stats()` (brief), verify minimal response shape. Call `stats(detail=true)`, verify full response with counts, staleness distribution, storage size.

### Golden Set Retrieval Test

A small hand-crafted test set (~20 memories + ~10 queries with expected results) to measure baseline retrieval quality and catch regressions. Not a full benchmark suite — just enough to verify that hybrid retrieval + reranking returns sensible results.

```
golden_set/
├── memories.json    # 20 memories of mixed types
├── queries.json     # 10 queries with expected memory IDs (top-3)
└── run_golden.rs    # stores memories, runs queries, reports recall@3
```

Target: recall@3 >= 0.7 on the golden set. If a code change drops below this, investigate before merging.

### MCP Protocol Tests

- Verify all 5 tools register correctly via MCP Inspector
- Verify tool parameter validation (required fields, type checks)
- Verify error responses have correct MCP error format
- Verify stdio transport round-trip (JSON-RPC request → response)

## Implementation Phases

### Phase 1: Foundation (~900 LOC)
- Scaffold workspace and crates
- Qdrant sidecar lifecycle management (start/stop/health check) + `--qdrant-url` external mode
- fastembed wrapper (embed + rerank, configurable model tiers)
- Qdrant client (vector + BM25 write/search via gRPC)
- SQLite schema (entity table, metadata tables, temporal validity columns)
- Basic `store` (all types) and `recall` (vector-only) MCP tools
- stdio transport via rmcp

### Phase 2: Reranking + Hybrid Retrieval (~400 LOC)
- Cross-encoder reranking (top-20 candidates) with multiplicative recency boosts (ferrex-embed: raw scores, ferrex-core: boost computation)
- BM25 via Qdrant built-in sparse index (server-side tokenization + IDF)
- Hybrid retrieval via Qdrant Query API: prefetch dense + sparse, server-side RRF (k=60) in one request

### Phase 3: Conflict Resolution + Entity Resolution (~600 LOC)
- `store` semantic type: conflict detection with temporal validity (`t_valid`/`t_invalid`)
- Contradiction detection at query time
- Entity resolution: full layered pipeline (normalize → fuzzy → embedding similarity → alias table)
- Entity storage (SQLite + Qdrant payload) with resolution
- Entity-based filtering on `recall`
- Deduplication on write

### Phase 4: Memory Lifecycle + Staleness (~600 LOC)
- `forget`, `stats`, `reflect` (staleness audit + contradiction alerts) tools
- Staleness scoring (age + access + validation recency)
- Staleness levels (fresh/aging/stale) with configurable thresholds
- Access-time validation refresh
- Freshness metadata annotation on recall results
- Decay scoring, eviction policy

### Phase 5: Polish (~300 LOC)
- Semantic caching on recall (LRU, embed-hash keyed)
- Nix build with ONNX runtime + Qdrant binary
- Integration tests
- MCP Inspector validation
- Retrieval quality instrumentation (for measuring v2 improvements)
