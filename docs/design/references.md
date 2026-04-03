# ferrex Design References

Papers, benchmarks, and sources that informed the design. Organized by topic.

## RAG Architectures & Surveys

- **"Agentic Retrieval-Augmented Generation: A Survey on Agentic RAG"**
  Aditi Singh, Abul Ehtesham, Saket Kumar, Tala Talaei Khoei. arXiv:2501.09136, January 2025.
  Formalizes agentic RAG as finite-horizon POMDPs. Taxonomy of architectures by planning, retrieval orchestration, memory paradigms, tool invocation.
  *Used for:* overall architecture validation, retrieval pipeline design.

- **"A Comprehensive Survey of Retrieval-Augmented Generation (RAG)"**
  arXiv:2410.12837, October 2024.
  Traces evolution from Lewis et al.'s original RAG through Naive → Advanced → Modular RAG.
  *Used for:* understanding RAG generation landscape.

- **"Memory in the Age of AI Agents"**
  Yuyang Hu et al. arXiv:2512.13564, December 2025. 107 pages, 47 authors.
  Taxonomizes memory along temporal scope, cognitive mechanism, and memory subject axes. Companion paper list: github.com/Shichun-Liu/Agent-Memory-Paper-List.
  *Used for:* memory type taxonomy, episodic/semantic/procedural separation rationale.

## Memory Type Taxonomy

- **"Position: Episodic Memory is the Missing Piece for Long-Term LLM Agents"**
  Pink, Wu, Vo, Turek, Mu, Huth, Toneva. arXiv:2502.06975, February 2025.
  Argues current agent memory is predominantly semantic; episodic memory is underexplored. Core challenges: event segmentation, temporal ordering, episode consolidation.
  *Used for:* justifying episodic memory as first-class type.

- **"AriGraph: Learning Knowledge Graph World Models with Episodic Memory for LLM Agents"**
  IJCAI-25 proceedings, 2025.
  Agents construct memory graphs integrating semantic and episodic memories. Outperforms baselines in text-based game environments.
  *Used for:* validating hybrid episodic + semantic approach.

- **"REMem: Reasoning with Episodic Memory in Language Agent"**
  OpenReview, 2025.
  Two-phase framework: offline indexing converting experiences into hybrid memory graph, online reasoning via episodic recollection.
  *Used for:* episodic memory retrieval patterns.

- **"ProcMEM"**
  Hugging Face Daily Papers, 2025.
  Formalizes procedural memory as Skill-MDP transforming passive episodic narratives into executable Skills with activation/execution/termination conditions.
  *Used for:* procedural memory design (conditions + versioned steps).

- **"MIRIX"**
  2025.
  Introduces six memory types (Core, Episodic, Semantic, Procedural, Resource, Multimodal). Argues three-type split is insufficient.
  *Used for:* challenging the three-type taxonomy (noted in future-improvements.md).

- **Google Titans Architecture**
  Referenced in multiple 2025 discussions.
  Three explicit memory layers: short-term (windowed attention), long-term (neural module rewriting during inference), persistent (learnable, time-independent). Prioritizes "surprising" information for long-term storage.
  *Used for:* informing decay/staleness design.

- **MemEvolve**
  2025.
  Meta-evolutionary framework jointly evolving agents' knowledge and memory architecture. Argues human-inspired taxonomies may not be optimal for AI agents.
  *Used for:* Decision #13 (type auto-detection), challenging rigid taxonomy.

## Hybrid Retrieval & Fusion

- **"An Experimental Analysis of Trade-offs in Hybrid Search"**
  arXiv:2508.01405, August 2025.
  Evaluates 4 single-path methods and all 11 hybrid combinations. RRF is score-agnostic and robust across domains. Introduces Tensor-based Rank Fusion (TRF).
  *Used for:* validating vector + BM25 + RRF as retrieval backbone.

- **"A Rank Fusion Framework for Enhanced Sparse Retrieval using LLM-Based Query Expansion"**
  ACL Findings 2025.
  Exp4Fuse: LLM-based zero-shot query expansion fused with RRF. 38% improvement in MAP@10 over BM25 alone.
  *Used for:* RRF effectiveness evidence.

- **Dynamic/Learned Fusion Weights**
  Hsu et al., March 2025.
  Query-specific adjustment of fusion weights via auxiliary LLM. Per-query optimal alpha rather than static global weight.
  *Used for:* adaptive query routing design (deferred to v2).

## Retrieval Quality & Simplicity

- **"StructMemEval"**
  Zhou and Han. arXiv:2602.11243, February 2026.
  EMem and EMem-G (simple embedding retrieval) outperform complex memory structures on LOCOMO and LongMemEval. Complex architectures only win on tasks requiring explicit memory organization.
  *Used for:* justifying v1 simplification (vector + BM25 only), deferring kNN/graph channels.

- **GraphRAG-Bench**
  June 2025.
  Graph retrieval is 13.4% less accurate than vanilla RAG on single-hop factoid queries.
  *Used for:* deferring graph expansion as retrieval channel to v2.

## Context-Enriched Embedding

- **Anthropic Contextual Retrieval**
  Anthropic blog, September 2024.
  Prepends LLM-generated chunk-specific context (50-100 tokens) before embedding. Combined with Contextual BM25: 49% reduction in retrieval failures, 67% with reranking.
  *Used for:* context-enriched embedding design. Note: our metadata prefix `[type | namespace | date]` is a simpler variant — benchmarking against alternatives tracked as Open Question #5.

- **"HeteRAG: A Heterogeneous RAG Framework"**
  Chen et al. arXiv:2504.10529, April 2025.
  Decouples retrieval and generation representations. Context-enriched modeling for retrieval, standalone chunks for generation. Adaptive prompt tuning for alignment.
  *Used for:* validating "prefix for embedding only, clean content stored" approach.

- **"Utilizing Metadata for Better Retrieval-Augmented Generation"**
  ECIR 2026, People/CS/VT.
  Dual-encoder with unified embeddings (metadata + content vectors summed) matches or exceeds text prefixing. Company and year metadata act as strong disambiguators.
  *Used for:* Open Question #5 (benchmark prefix format alternatives).

## Cross-Encoder Reranking

- **"Jointly Comparing Multiple Candidates for Efficient and Effective Retrieval"**
  EMNLP 2024.
  CMC (Compare Multiple Candidates): 3-stage pipeline (bi-encoder + CMC + cross-encoder) is more accurate AND faster than bi-encoder + cross-encoder. CMC compares ~10K candidates in time cross-encoder processes 16.
  *Used for:* future intermediate reranker consideration.

- **"MICE: Minimal Interaction Cross-Encoders for Efficient Re-ranking"**
  arXiv:2602.16299, 2025.
  Pre-computes document tokens offline for dramatically faster cross-encoder inference.
  *Used for:* future reranking optimization.

- **Cross-Encoder RAG Accuracy Study**
  Ailog, 2025.
  Cross-encoder reranking improves RAG accuracy by ~40% overall. +18% fact lookups, +47% multi-hop, +52% complex queries. Latency: 200ms-2s, sweet spot 50-75 candidates.
  *Used for:* justifying always-on reranking, multiplicative boost design.

## Temporal Awareness & Staleness

- **MemoryBank**
  Zhong et al., 2024.
  Ebbinghaus-inspired forgetting curve for dynamic memory updates. Limitation: temporal decay hurts when old memories remain relevant (names, preferences).
  *Used for:* type-specific decay design (semantic: no time decay, only validation staleness).

- **"A Knowledge-Grounded Cognitive Runtime for Trustworthy AI Agents"**
  arXiv:2603.25097, 2025.
  Three timestamps per fact (event, ingestion, update). Graph edges carry valid-from/valid-until. Consolidation engine uses ingestion timestamps for stale knowledge detection.
  *Used for:* bi-temporal model design, staleness safeguard layers.

- **Zep/Graphiti Bi-Temporal Model**
  arXiv:2501.13956, January 2025.
  Temporal Knowledge Graph with valid time + transaction time. Neo4j-backed. Hybrid retrieval (semantic + BM25 + graph + RRF). 94.8% on DMR benchmark.
  *Used for:* `t_valid` / `t_invalid` design on semantic facts.

## Knowledge Graphs vs Vector Stores

- **Machine Learning Mastery Comparison**
  2025.
  Vector databases fail at multi-step logic; knowledge graphs fail at fuzzy semantic matching. Winning pattern: hybrid.
  *Used for:* validating future KG + vector hybrid design (v2).

- **"GRAG: Graph Retrieval-Augmented Generation"**
  NAACL Findings 2025, Emory University.
  Integrates graph context into retrieval and generation for networked documents.
  *Used for:* future graph expansion retrieval design.

- **Contrarian: Embedding + Graph Bottlenecks**
  arXiv:2602.13594, February 2026.
  Both embedding-based and graph-based systems introduce significant performance bottlenecks in agentic workflows.
  *Used for:* justifying v1 simplicity, deferring graph retrieval.

## Graph Libraries & Scale

- **"VSAG: An Optimized Search Framework for Graph-based ANN Search"**
  VLDB 2025.
  HNSW limitations: 67% L3 cache miss rate consuming 63% of search time. Proposes redundant vector storage with sequential memory access.
  *Used for:* understanding HNSW overhead in Qdrant.

- **petgraph Benchmark Study**
  arXiv:2502.13862, February 2025.
  Compares petgraph against SNAP, GraphBLAS, cuGraph, Aspen. petgraph 87x slower than fastest for batch updates. Sequential only — no parallel processing.
  *Used for:* Decision #7 (dropping petgraph, SQLite-only graph storage).

## Chunking

- **Chunking Accuracy Benchmarks**
  Multiple sources, 2024-2025.
  54% accuracy for fragmented chunks vs 69% for intact content. Semantic chunking over-fragments short text (43-token average).
  *Used for:* Decision #11 (type-aware, mostly no chunking).

- **RAG Fusion Post-Reranking**
  arXiv:2603.02153, March 2026.
  RAG fusion increases raw recall but gains vanish after reranking.
  *Used for:* rejecting RAG fusion chunking strategy.

## Competitors & Benchmarks

- **Hindsight**
  vectorize-io, December 2025. MIT license.
  4 memory networks (World, Experience, Opinion, Entity/Observation) + multi-strategy retrieval + cross-encoder reranking. 91.4% LongMemEval, ~89.6% LOCOMO.
  *Used for:* self-contained memory format, kNN link concept (deferred), two-phase temporal retrieval (deferred).

- **Mem0 / Mem0g**
  ECAI 2025, arXiv:2504.19413.
  Vector + Graph variant. 91% latency reduction, 90%+ token savings. Graph variant: 58.13% temporal reasoning vs OpenAI's 21.71%. 48K GitHub stars.
  *Used for:* competitive positioning, understanding LLM-extraction tradeoffs.

- **Zep / Graphiti**
  arXiv:2501.13956, January 2025.
  Temporal Knowledge Graph, Neo4j-backed. Graphiti is MIT OSS. ~85% LOCOMO.
  *Used for:* bi-temporal model, competitive positioning.

- **Letta / MemGPT**
  ICLR 2024, rebranded.
  OS-inspired tiered memory. LLM manages own memory via function calls. "Sleeptime agents" for async processing (2025 update). ~83.2% LOCOMO.
  *Used for:* competitive positioning.

- **Cognee**
  KG + Vector + structured extraction. Focus on reducing hallucinations. 12K GitHub stars.
  *Used for:* competitive positioning.

- **A-Mem**
  arXiv:2502.12110, NeurIPS 2025.
  Zettelkasten-style linked notes. 85-93% token reduction.
  *Used for:* Decision #14 (deduplication on store), competitive positioning.

- **Memobase.ai**
  MCP-native memory-as-a-service. PostgreSQL with row-level security. Cross-tool "memory passport."
  *Used for:* competitive positioning.

- **EverMemOS** — 92.3% on LOCOMO. Proprietary.
- **MemMachine** — 91.7% on LOCOMO. Proprietary.
- **SuperLocalMemory V3** — 74.8% with zero cloud dependency, mathematical retrieval only.
- **LangMem (LangChain)** — 58.10% LOCOMO, p95 latency 59.82s.

## Benchmarks Referenced

- **LOCOMO** — Long-context memory benchmark. Used by most competitors for comparison.
- **LongMemEval** — Long-term memory evaluation. Hindsight's primary benchmark (91.4%).
- **MTEB** — Massive Text Embedding Benchmark. Used for embedding model selection.
- **BEIR** — Benchmark for information retrieval. nDCG@10 used for reranker comparison.
- **DMR** — Dynamic Memory Retrieval benchmark. Zep/Graphiti scored 94.8%.

## Embedding & Reranker Models

- **BGE-base-en-v1.5** — Selected default. 768d, 210MB, MTEB ~63. MIT license.
- **BGE-M3** — Unified dense + sparse + ColBERT. 1024d, ~1.5GB, 100+ languages. Future evaluation candidate.
- **Qwen3-Embedding-0.6B** — New MTEB leader at small size. Apache-2.0. Behind fastembed feature flag.
- **EmbeddingGemma-300M** — Google, 308M params, 200MB quantized. MMTEB state-of-the-art.
- **Jina Reranker v3** — 0.6B, BEIR nDCG@10 61.94. "Last but not late interaction" on Qwen3 backbone. Recommended reranker.
- **ms-marco-MiniLM-L-12-v2** — 120MB, BEIR ~40. Fallback reranker if Jina unavailable in fastembed-rs.

## BM25 & Sparse Vectors

- **Qdrant BM25 Support** — First-class since v1.15. IDF modifier at query time. Sparse vector storage.
- **SPLADE** — Learned sparse model. 19% quality loss on domain shift (MS MARCO → e-commerce). GPU required.
- **Qdrant BM42** — July 2024. Experimental BM25 + transformer hybrid.
  *Used for:* choosing Qdrant native BM25 over SPLADE for v1.

## SQLite

- **SQLite WAL Mode** — Unlimited concurrent readers, single writer. Practical ceiling ~100 writes/sec.
- **SQLite WAL Bug** — Data race in versions 3.7.0-3.51.2, fixed in 3.51.3 (March 2026). Tight timing, unlikely but possible.
  *Used for:* understanding SQLite limitations for concurrent access.

## MCP & Tooling

- **rmcp** — Official Rust MCP SDK. `#[tool]` macros, stdio transport.
- **fastembed** — 44 embedding + 6 reranker models, ONNX quantized. Maintained by Qdrant team.
- **MCP Context Window Tax** — Research shows MCP tools consume 16%+ of context. Fewer, richer tools preferred.
  *Used for:* 5-tool API surface decision.
