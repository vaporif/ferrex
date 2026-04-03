# ferrex Design Decisions

Decisions made during design review, April 2026.

## 1. kNN Links: Bidirectional Insert + Cap at 10

**Problem:** kNN links computed at insert time are forward-only — older memories never discover newer, better neighbors.

**Decision:** When memory B finds A as a neighbor, also add B to A's link list. Cap each memory's link list at 10, evict the weakest link when exceeded.

**Why:** Simplest fix, maintains freshness naturally, no background jobs or retrieval latency hit. Extra writes are negligible.

## 2. Recall Without Query: Smart Default

**Problem:** "Call recall at START of conversation" — but the agent has no query yet.

**Decision:** Make `query` optional. When omitted, return a smart default based on:
1. Semantic facts about the current project (detected from MCP workspace roots)
2. Most recent episodic memories
3. Any stale/contradicted facts needing attention

**Why:** Clean UX, single tool, agent just calls `recall()` with no args. MCP clients expose workspace roots that identify the project.

## 3. Entity Name Fragmentation: Alias Table

**Problem:** Agents store inconsistent entity names ("tokio" vs "Tokio" vs "tokio runtime").

**Decision:** Each entity has a canonical name + list of aliases. On entity creation:
1. Normalize (lowercase, trim) → check for exact match
2. Fuzzy match against existing entities (SequenceMatcher ratio > 0.85) → merge
3. Embedding similarity > 0.92 → merge
4. Embedding similarity 0.80-0.92 → store both, add as alias candidates, surface in `reflect`

**Why:** Layered approach uses the right tool for each case. Deterministic for obvious matches, embedding-based for semantic equivalence, human review for ambiguous cases.

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

## 6. Qdrant Sidecar: PID File + Connect-or-Start

**Problem:** Orphan processes, concurrent instances, startup latency, port conflicts.

**Decision:**
1. On startup, check PID file at `~/.ferrex/qdrant.pid`
2. If PID file exists and process is alive → reuse (connect to existing)
3. If PID file exists but process is dead → clean up, start fresh
4. If no PID file → start Qdrant, write PID file
5. Connect with retry + backoff (up to 5s)
6. On ferrex exit → if we started Qdrant (not reused), send SIGTERM
7. Data directory: `~/.ferrex/qdrant-data/` (deterministic, per-user)

**Why:** Handles orphans (step 3), concurrent instances (step 2 — share the sidecar), startup latency (step 5), clean shutdown (step 6).

## 7. SQLite + petgraph Consistency: SQLite as Source of Truth

**Problem:** No transaction boundary spans both SQLite and petgraph. Crash between writes leaves them inconsistent.

**Decision:** SQLite is the authoritative store. petgraph is a read-only cache rebuilt from SQLite on startup. Write ordering: SQLite first, petgraph second. If ferrex crashes after SQLite write but before petgraph update, restart rebuilds petgraph from SQLite.

**Why:** Simple, correct, already implied by the design. The only "inconsistency" window is within a single crashed request, which is dead anyway.

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

## 12. Context-Enriched Embedding

**Problem:** Embedding bare content loses type, project, and temporal context. Two memories from different projects about "connection pool" are indistinguishable in embedding space.

**Decision:** Prepend metadata to content before embedding: `[{type} | {namespace} | {date}] {content}`. Applied to both stored memories and recall queries for consistency. The prefix is for embedding only — stored content remains clean.

**Why:** Anthropic's Contextual Retrieval research reported 67% reduction in retrieval failures. No LLM needed — just string concatenation. Cheapest possible retrieval quality improvement.
