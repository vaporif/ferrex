# ferrex Design Decisions

Decisions made during design review, April 2026.

## 1. kNN Links: Deferred to v2

**Original design:** Bidirectional insert + cap at 10. When memory B finds A as a neighbor, also add B to A's link list.

**Decision:** Deferred to v2. See `future-improvements.md`.

**Why:** Unproven contribution in isolation. Query-time second-pass search achieves similar results with zero insert cost. Add when retrieval quality measurements show recall misses that kNN expansion would fix.

## 2. Recall: Query Required, No Smart Default

**Problem:** "Call recall at START of conversation" — but the agent has no query yet.

**Original decision:** Make `query` optional with a smart default (project facts + recent episodics + stale items).

**Revised decision:** Keep `query` required on `recall`. The "what should I know?" use case is handled by `stats`, which already returns system health and can surface top recent memories + items needing attention. Overloading `recall` with two modes (search vs dashboard) creates ambiguous response shapes and scope creep.

**Why:** Single-responsibility. `recall` searches, `stats` summarizes. The agent provides a query like "project context" or "recent decisions" rather than ferrex guessing what's relevant.

## 3. Entity Name Fragmentation: Full Layered Resolution

**Problem:** Agents store inconsistent entity names ("tokio" vs "Tokio" vs "tokio runtime").

**Decision:** Each entity has a canonical name + list of aliases. Full layered pipeline in v1:
1. Normalize (lowercase, trim, collapse separators) → check for exact match → merge
2. Fuzzy match against existing entities (SequenceMatcher ratio > 0.85) → merge
3. Embedding similarity > 0.92 → merge
4. Embedding similarity 0.80-0.92 → store both, add as alias candidates, surface in `reflect`
5. No match → create as new entity

All lookups check aliases first. This is the full pipeline — not deferred.

**Why:** Layered approach uses the right tool for each case. Deterministic for obvious matches, embedding-based for semantic equivalence, human review for ambiguous cases. Entity fragmentation compounds over time and is harder to fix retroactively than to prevent upfront.

## 4. Reflect: Agent-Side LLM (ferrex stays LLM-free)

**Problem:** Promoting episodic → semantic requires understanding meaning. Pure clustering can't extract structured triples.

**Decision:** ferrex clusters episodic memories and returns structured suggestions in the tool response. The calling agent (which is an LLM) does the reasoning and decides whether to call `store(type: "semantic", ...)`.

```json
{
  "clusters": [
    {
      "theme": "connection pool issues",
      "memories": ["mem_7", "mem_12", "mem_22"],
      "shared_entities": ["connection-pool", "api-server"],
      "suggestion": "These 3 memories over 2 weeks involve connection-pool + api-server. Consider storing a semantic fact."
    }
  ],
  "stale": [...],
  "contradictions": [...]
}
```

**Why:** ferrex remains LLM-free. The LLM work happens in the agent's context, not in ferrex.

**TODO:** Add optional LLM integration (e.g., Ollama) for users who want automated triple extraction without agent round-trips. This is a v2 feature, not a blocker.

## 5. Conflict Resolution: Two-Stage Comparison

**Problem:** Embedding similarity on short strings is unreliable. "tokio 1.36" vs "tokio 1.38" might wrongly deduplicate.

**Decision:** Two-stage comparison on the object field:
1. Exact match after normalization → deduplicate
2. Fuzzy string match (ratio > 0.95) → deduplicate
3. Fuzzy string match (ratio < 0.5) → definitely different, update + invalidate old
4. Middle ground (0.5-0.95) → embed both full facts (including subject+predicate for context), if cosine > 0.95 deduplicate, otherwise update + invalidate

**Why:** Uses string comparison where it's reliable, embeddings only as a tiebreaker for ambiguous cases.

**Predicate normalization:** Conflict matching uses normalized predicates, not raw strings. A static synonym map groups common equivalents ("uses"/"depends-on"/"requires" → `depends_on`), with fuzzy matching (ratio > 0.85) as fallback. Without this, ("api-server", "uses", "tokio 1.36") and ("api-server", "depends-on", "tokio 1.38") would be treated as unrelated facts instead of a version update. See main design doc Conflict Resolution section for the full predicate normalization pipeline.

## 6. Qdrant Connection: Sidecar or External URL

**Problem:** Orphan processes, concurrent instances, startup latency, port conflicts. Also: some users already run Qdrant or prefer to manage it themselves.

**Decision:** Two modes, selected by presence of `--qdrant-url`:

**Sidecar mode (default, no flag):**
1. On startup, check PID file at `~/.ferrex/qdrant.pid`
2. If PID file exists and process is alive → reuse (connect to existing)
3. If PID file exists but process is dead → clean up, start fresh
4. If no PID file → start Qdrant, write PID file
5. Connect with retry + backoff (up to 5s)
6. On ferrex exit → if we started Qdrant (not reused), send SIGTERM
7. Data directory: `~/.ferrex/qdrant-data/` (deterministic, per-user)

**External mode (`--qdrant-url <url>`):**
1. Skip all sidecar lifecycle management
2. One connection attempt, 3-second timeout
3. On failure: clear error message and exit (no retry — user manages the instance)

**Why:** Sidecar handles orphans (step 3), concurrent instances (step 2 — share the sidecar), startup latency (step 5), clean shutdown (step 6). External URL mode enables team deployments, existing infrastructure reuse, and avoids sidecar overhead entirely when the user prefers to manage Qdrant themselves.

## 7. SQLite as Sole Graph Store (petgraph deferred)

**Original design:** SQLite as source of truth + petgraph as in-memory read cache, rebuilt on startup.

**Decision:** Use SQLite with proper indexes as the only graph store. No petgraph in v1.

**Why:** Sub-millisecond SQLite queries are invisible next to 200ms+ reranking latency. petgraph added a consistency problem and startup rebuild cost for a performance benefit lost in pipeline noise. If graph traversal becomes a retrieval channel in v2 and SQLite latency is measurable, revisit petgraph then. See `future-improvements.md`.

## 8. Recency Boost: Type-Specific with Half-Life Decay

**Problem:** Linear recency formula has no floor, penalizes old memories arbitrarily. Procedural memories are timeless but get penalized.

**Decision:** Type-specific multiplicative boosts using half-life decay:
- Episodic: `1.0 + 0.1 * 2^(-age_days/30)` (half-life 30 days, range 1.0-1.1)
- Semantic: `1.0 + 0.05 * 2^(-age_days/180)` (half-life 180 days, range 1.0-1.05)
- Procedural: `1.0` (no boost)

**Why:** Each memory type has different temporal semantics. Exponential decay is mathematically clean, never goes negative, and is configurable per type.

## 9. Tool Description Priming: Layered Reinforcement

**Problem:** Tool descriptions as behavioral instructions work ~70% of the time. Not reliable enough alone.

**Decision:** Three reinforcement layers:
1. **MCP server `instructions`** field: session-level behavior ("ferrex is your long-term memory. Call recall at conversation start. Call store when you learn new facts. Call reflect periodically.")
2. **Tool descriptions**: per-tool guidance (directive language like "Call this at the START of every conversation")
3. **Response hints**: situational nudges in recall/reflect responses (e.g., "Memory contains 3 stale facts. Consider running reflect.")

**Why:** No single mechanism is reliable. Layering server instructions + tool descriptions + response hints maximizes compliance without requiring hooks.

## 10. Missing Design Elements

### Embedding Model Migration
- Store model name + version in SQLite metadata table
- On startup, if configured model != stored model, warn and refuse to start
- `ferrex re-embed` CLI command re-embeds all memories with the new model
- No auto-migration — it's destructive and slow

### Memory Scoping / Namespacing
- Optional `namespace` parameter on all tools (default: inferred from MCP workspace root, or `"default"`)
- Qdrant payload filter on namespace
- SQLite queries filter on namespace
- Agent on project A doesn't see project B's memories unless explicitly asked

### Content Size Limits
- Max content length per memory: 4096 chars (configurable)
- Reject with error on store if exceeded
- Prevents agents from dumping entire files as "memories"

### Backup / Restore
- SQLite: copy the file
- Qdrant: snapshot API (`POST /collections/{name}/snapshots`)
- `ferrex backup` CLI command does both atomically
- `ferrex restore` to reload from backup

## 11. Chunking: Type-Aware, Step-Boundary Splitting

**Problem:** Memories can exceed the embedding model's context window (512 tokens for BGE-base). Need a chunking strategy that doesn't destroy context.

**Decision:** Don't chunk most memories. Type-specific rules:
- **Episodic**: never chunk. Reject if too long — enforce self-contained format.
- **Semantic**: never chunk. Triples are always short.
- **Procedural**: chunk on step boundaries (not token counts) when content exceeds model context. Each step gets its own vector, all sharing the same memory_id with a step_index.

**Why:** ferrex stores memories, not documents. Most memories are short (50-300 chars). Chunking short text destroys context — benchmarks show 54% accuracy for fragmented chunks vs 69% for intact content. Procedural memories have natural step boundaries that are better split points than arbitrary token windows.

**Rejected alternatives:** Sliding window (memories aren't documents), semantic chunking (over-fragments short text), late chunking (solves cross-reference problems we don't have), propositional chunking (requires LLM, our memories are already near-atomic), RAG fusion (gains vanish after reranking per arXiv:2603.02153).

## 12. Context-Enriched Embedding → Superseded by Decision #18

**Original decision:** Prepend `[{type} | {namespace} | {date}]` metadata to content before embedding.

**Superseded by Decision #18:** Plain-text embedding + Qdrant payload filtering. The metadata prefix was found to be out-of-distribution for BGE-base and likely harmful. Anthropic's Contextual Retrieval uses LLM-generated prose, not structured metadata tokens. See Decision #18 for full rationale.

## 13. Memory Type Auto-Detection

**Problem:** Forcing agents to classify memories as episodic/semantic/procedural at write time adds cognitive burden. Agents mis-classify — a fact can be both episodic (it happened) and semantic (it's a durable truth). Research (MemEvolve 2025) questions whether human cognitive categories are optimal for AI agents at all.

**Decision:** `type` is optional on `store`. When omitted, auto-detect from provided fields:
- `subject` + `predicate` + `object` present → semantic
- `steps` or `conditions` present → procedural
- Everything else → episodic

Agent can still set type explicitly if it wants to override.

**Why:** Shifts classification burden from agent to system. The field structure already implies the type unambiguously. Keeps the type system as an internal optimization detail while preserving backward compatibility for explicit callers.

## 14. Deduplication on Store

**Problem:** Agents over-store — the same fact gets stored repeatedly with slight rewording. Without deduplication, memory fills with near-identical entries that dilute retrieval quality.

**Decision:** Before writing, embed the incoming memory and search existing same-type memories in Qdrant. If cosine similarity exceeds a configurable threshold (default 0.95) → reject with `"similar memory already exists: {id}"`. One extra Qdrant search per store call. Threshold configurable via `ferrex.toml` (`deduplication.threshold`) or `--dedup-threshold` CLI flag.

**Why:** Cheapest defense against the most common memory bloat pattern. The default 0.95 is conservative — only rejects near-exact duplicates. The `supersedes` param bypasses this check for intentional updates. A-Mem (NeurIPS 2025) showed 85-93% token reduction by controlling what gets stored; this is the minimal version of that idea. The optimal threshold depends on the embedding model and content length distribution — empirical tuning on real workloads is expected. Making it configurable avoids baking in an untested assumption.

## 15. Server-Side RRF via Qdrant Query API

**Original design:** Client-side RRF — two separate Qdrant queries (dense + sparse) via `tokio::join!`, then merge results in ferrex with custom RRF implementation.

**Decision:** Use Qdrant's Universal Query API (available since v1.10) with `prefetch` stages for dense and sparse retrieval, fused via `Fusion::RRF` server-side in a single request.

**Why:** Eliminates ~200 LOC of client-side RRF code. One round-trip instead of two. Server-side fusion is more efficient (Qdrant can optimize internally). Also eliminates the imbalanced-result-list problem (where one channel returns fewer results than another) — Qdrant handles this internally.

## 16. BM25 Tokenization: Server-Side (No Client-Side Sparse Vectors)

**Decision:** Send raw text to Qdrant for BM25 indexing. Qdrant computes TF-based sparse vectors server-side and maintains collection-level IDF via `Modifier::IDF` on `SparseVectorParams` (available since v1.15).

**Why:** ferrex doesn't need any BM25/tokenization logic. The ingestion pipeline sends text once; Qdrant handles both dense vector storage and sparse/BM25 indexing. This simplifies `ferrex-store/qdrant.rs` — one write path, no client-side tokenizer dependency.

## 17. Reranking Pool: Top-20 (Not Top-10)

**Original design:** Rerank top-10 candidates after RRF.

**Decision:** Rerank top-20 candidates. Research consensus is 50-75 for large corpora; for small personal memory corpora, 20 is the sweet spot.

**Why:** Top-10 risks missing relevant results that RRF ranked low but the cross-encoder would promote. Cross-encoder latency on 20 candidates with quantized ONNX is negligible (~50ms). The quality improvement outweighs the minimal latency cost.

## 18. No Embedding Prefix — Use Qdrant Payload Filtering

**Original design:** Prepend `[type | namespace | date]` to content before embedding, inspired by Anthropic's Contextual Retrieval.

**Decision:** Embed plain text only. Use Qdrant payload filtering for type, namespace, and temporal constraints.

**Why:** BGE-base-en-v1.5 expects plain text on the document side. The `[type | namespace | date]` format is out-of-distribution — structured metadata tokens shift vectors unpredictably and waste the 512-token budget. Anthropic's Contextual Retrieval uses LLM-generated natural language prose (50-100 tokens of semantic context), which is a fundamentally different technique. For ferrex's use case, Qdrant payload filtering achieves the same scoping with zero embedding quality degradation.

## 19. `forget` Tool: ID-Only (No Query-Based Deletion)

**Original design:** `forget` accepted either `ids` (targeted) or `query` (search-and-delete with "confirmation").

**Decision:** `forget` requires explicit `ids` only. No query-based batch deletion.

**Why:** MCP tools can't prompt for interactive confirmation — they return results, not input dialogs. Query-based deletion with no real confirmation mechanism is a mass-delete footgun. The agent can `recall` first to find IDs, then `forget` specific ones. This is safer and simpler. Query-based batch delete can be a v2 convenience feature if needed.

## 20. Reranker Tiers: Use fastembed Built-Ins

**Original design:** Listed ms-marco-MiniLM and jina-reranker-v3 as reranker tiers.

**Decision:** Use fastembed built-in rerankers only. Default: `bge-reranker-base`. Multilingual: `jina-reranker-v2-base-multilingual`.

**Why:** ms-marco-MiniLM and jina-reranker-v3 are not fastembed built-ins. They require `UserDefinedRerankingModel` with manually sourced ONNX files or direct `ort` crate loading — extra complexity for v1. The built-in `bge-reranker-base` is a solid default. Evaluate jina-v3 via custom loading post-v1 if quality is insufficient.

## 21. Retrieval ≠ Validation: Separate Signals

**Original design:** Retrieving a memory via `recall` bumps `last_validated`, creating a feedback loop where frequently-retrieved memories stay fresh.

**Problem:** ferrex can't distinguish "agent read this and found it useful" from "agent read this and ignored it." Treating retrieval as implicit validation means popular-but-wrong memories never age out — the same failure mode we criticize in other systems.

**Decision:** `last_accessed` and `last_validated` are separate signals:
- `last_accessed` bumped on every `recall` hit. Used for decay scoring (popularity signal).
- `last_validated` bumped only by explicit agent actions: `store(supersedes: id)`, `reflect` confirmation, `store(source: "memory:id")`.

**Why:** Retrieval is a popularity signal, not a correctness signal. Separating them means stale-but-popular memories still drift toward staleness based on validation age, while frequently-used memories get a mild decay benefit without false freshness.

## 22. Stats Brief/Detail Modes

**Original design:** `stats` returns full diagnostics (counts, staleness distribution, storage size, recent memories, needs_attention) on every call.

**Problem:** Agents call `stats` at conversation start. Full diagnostics waste tokens on information irrelevant to 90% of conversations (storage_mb, entity count, full staleness breakdown).

**Decision:** `stats` has a `detail` parameter (default `false`):
- **Brief mode** (default): returns `total` count, top-5 `recent` memories, and `needs_attention` section only.
- **Detailed mode** (`detail=true`): returns full diagnostics including counts by type, staleness distribution, storage size, entity count.

**Why:** Brief mode gives the agent enough context to orient (recent memories + what needs attention) without burning tokens on system health metrics. Detailed mode available on demand for health checks and debugging.

## 23. Predicate Normalization for Conflict Detection

**Problem:** Agents use inconsistent predicates ("uses" vs "depends-on" vs "requires"). Conflict detection on exact (subject, predicate) match misses obvious contradictions when the same relationship is expressed with different predicate wording.

**Decision:** Normalize predicates before conflict matching:
1. Lowercase, trim, collapse separators
2. Static synonym map for common predicate families (extensible via config)
3. Fuzzy match (SequenceMatcher ratio > 0.85) against existing predicates for the same subject

**Why:** Without predicate normalization, ("api-server", "uses", "tokio 1.36") and ("api-server", "depends-on", "tokio 1.38") are treated as unrelated facts instead of a version update. The synonym map handles the common cases cheaply; fuzzy matching catches the rest.

## 24. Configurable Staleness Thresholds (Per-Namespace)

**Original design:** Type-based staleness thresholds only (episodic: 90d, semantic: 180d, procedural: 365d).

**Problem:** Type-based thresholds are a proxy for domain-specific staleness. A CI/CD workflow (procedural) could go stale in a week. A historical fact (semantic) like "company founded in 2020" never goes stale. One-size-fits-all-per-type doesn't capture this.

**Decision:** Type-based defaults remain, but namespaces can override thresholds via `ferrex.toml`. Fast-moving projects set aggressive thresholds; stable reference projects relax them.

**Why:** Per-namespace overrides let users tune staleness to their domain without adding per-memory complexity. The type-based defaults are reasonable fallbacks for users who don't configure anything.
