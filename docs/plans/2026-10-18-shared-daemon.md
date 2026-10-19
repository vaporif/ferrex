# Shared ferrex daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or `/team-feature` to implement this plan task-by-task, per the Execution Strategy below. Steps use checkbox (`- [ ]`) syntax — these are **persistent durable state**, not visual decoration. The executor edits the plan file in place: `- [ ]` → `- [x]` the instant a step verifies, before moving on. On resume (new session, crash, takeover), the executor scans existing `- [x]` marks and skips them — these steps are NOT redone. TodoWrite mirrors this state in-session; the plan file is the source of truth across sessions.

**Goal:** Run one ferrex daemon per (binary, resolved config) that holds the models, SQLite and the Qdrant client, with every Claude session reaching it through a thin stdio proxy that spawns it on demand.

**Architecture:** `ferrex` with no subcommand becomes a proxy. It hashes its exe and resolved config into a key, connects to `<run_dir>/<key>.sock` (spawning `ferrex … daemon --key <k>` under a flock if nothing listens), sends a one-line hello carrying the session namespace, then forwards newline-framed JSON-RPC both ways while tracking ids so it can reconnect and replay `initialize` after a daemon loss. The daemon runs one rmcp `FerrexServer` per connection over a shared `MemoryService`, idles out when the last session closes, and never stops the qdrant sidecar; it supervises it instead.

**Tech Stack:** Rust 2024, tokio, rmcp 3.5.1 (`transport-async-rw` over `UnixStream`), clap 4, serde/serde_json, `sha2`, `rustix` (`process`), `std::fs::File::lock`, cargo-nextest.

**Spec:** `docs/specs/2026-10-14-shared-daemon-design.md`. Executors read it alongside this plan. Wherever a step says "per spec §X", the spec section is the requirement and is not re-decided here.

## Units

```
unit            | purpose (1 line)                                        | interface                                                       | depends on                   | files
daemon-addr     | key, run-dir paths, live-daemon lookup by db or any     | DaemonPaths::for_config(&FerrexConfig, exe: &Path) -> Result<Self>; PidFile read/write; live_for_db(run_dir, db_path) -> Option<u32>; any_live(run_dir) -> Option<u32> | ferrex-core config (Serialize) | crates/server/src/daemon/addr.rs; Serialize derives in crates/core/src/{types,staleness}.rs, crates/embed/src/lib.rs
handshake       | hello/reply types, encode/decode, version+key check     | Hello, HelloReply{ok,kind,error}, Hello::check(key)             | serde                        | crates/server/src/daemon/handshake.rs
spawn           | connect-or-spawn under flock                            | connect_or_spawn(&DaemonPaths, exe, argv, &Budgets) -> Result<UnixStream, AttemptError>; run_round(resolve, attempt, used) -> Result<Connected, RoundError> | daemon-addr                  | crates/server/src/daemon/spawn.rs
daemon          | accept loop, hello, per-connection sessions, idle exit  | daemon::run(config, key) -> eyre::Result<()>                    | daemon-addr, handshake, server, ferrex-core | crates/server/src/daemon.rs
proxy           | stdio <-> socket line forwarding and startup            | proxy::run(config, argv) -> eyre::Result<()>                    | spawn, handshake, daemon-addr | crates/server/src/proxy.rs
reconnect       | daemon-loss handling inside the proxy                   | proxy::reconnect::{on_daemon_loss, replay(mode: Forward|Drop)} | spawn, handshake             | crates/server/src/proxy/reconnect.rs
server          | MCP tool handlers with per-session defaults             | FerrexServer::with_session(Arc<MemoryService>, SessionDefaults) | ferrex-core                  | crates/server/src/server.rs (existing)
core-namespace  | session fallback namespace for taxonomy and diagnostics | TaxonomyRequest default-namespace field; diagnostics(ns)        | ferrex-store                 | crates/core/src/service/{taxonomy,stats}.rs, types.rs (existing)
cli             | `daemon --key` subcommand, `--no-daemon`, routing       | Cli / Command                                                   | —                            | crates/server/src/cli.rs, main.rs (existing)
sidecar-policy  | daemons never stop qdrant; owner reaps and restarts it | QdrantSidecar::start(.., SidecarPolicy); MemoryService::from_config_with(config, policy) | ferrex-store sidecar | crates/store/src/sidecar.rs, crates/core/src/service.rs (existing)
admin-guard     | refuse write-admin commands while a daemon uses the DB  | admin_guard::refuse_if_live(&FerrexConfig) (same DB); refuse_if_any_live() (nuke on ~/.ferrex) | daemon-addr                  | crates/server/src/admin_guard.rs (new); calls in audit.rs, backfill.rs, nuke in commands.rs
```

```
daemon-addr
  fails when: socket path over sun_path → error naming the path
  proven by:  key changes with exe mtime, db_path and any config field; namespace doesn't change it; same inputs give the same key; a destructure of `FerrexConfig` with no `..` (adding a field breaks the build until the key code is reviewed); a config with several predicate groups hashed in two separate processes gives the same key (catches `serde_json/preserve_order` enabled via feature unification); `ferrex daemon-paths` from two different cwds, with relative and absolute spellings of the same --db-path, prints the same key; long-path error test; (T4) live_for_db finds a live daemon with another key on the same db and ignores stale PID files; (T4) any_live finds a daemon on any db
handshake
  fails when: malformed line, unknown protocol version, wrong key → ok:false with kind, connection closed
  proven by:  round-trip test; version mismatch test; key mismatch test
spawn
  fails when: daemon doesn't accept within the budget → error naming ferrexd.log
  proven by:  N `ferrex` proxies started concurrently against one run dir leave exactly one daemon (integration test); a stale socket file is replaced; a lock held by a killed process doesn't block; a lock held by a live process ends the round after one attempt with a flock-timeout error (`FERREX_TEST_PROXY_TIMEOUT_MS`); unit test of `run_round` with a stub `resolve` returning key A then key B and an `attempt` that refuses A with `kind:"key"`: the second attempt uses B and succeeds
daemon
  fails when: MemoryService init fails → kind:init to all hellos, then shutdown; key recomputed from its own config differs from --key → logs both, exits before binding; ensure_collection fails → kind:session to that connection only; socket already served by a live daemon → exits 0 without touching it; no hello within 10 s → connection closed
  proven by:  two clients served concurrently against one service; recall in a fresh namespace returns empty, not an error; init failure (DB pre-seeded with a different embedding_model) gives kind:init and the daemon exits; `ferrex daemon --key bogus` run directly exits before binding (and leaves no socket); idle timer fires with 0 connections, including when nobody ever connects; a connection arriving after the timer fires is served, not dropped (`FERREX_TEST_IDLE_PAUSE_MS`); shutdown never deletes another daemon's socket or PID file; a connection that sends a bad hello or nothing at all doesn't stop the daemon idling out; init failure keeps refusing with kind:init for 60 s, then exits; an invalid namespace in one hello gets kind:session while a concurrent session keeps working; `ferrex daemon --key <k>` run while `<k>` is already served exits 0 and leaves the live daemon's socket and PID file untouched; SIGTERM while a test holds `<key>.lock` exits within ~10 s and leaves the socket and PID files in place; an idle exit while the lock is held keeps serving; with `FERREX_TEST_INIT_DELAY_MS` longer than `FERREX_INIT_TIMEOUT`, init times out into kind:init and the error text says timeout
proxy
  fails when: before initialize, a failed round → stderr + exit non-zero; spawned child exits early → failure pointing at ferrexd.log
  proven by:  end-to-end MCP initialize + tools/list + store/recall through the proxy; with `FERREX_TEST_DAEMON_EXIT_BEFORE_BIND=1`, the proxy notices the child exit, releases the lock and fails the attempt well inside the 30 s accept budget; against the fake daemon: three key refusals end the round and the proxy exits non-zero, a protocol or init refusal exits after one attempt, EOF before the hello reply is retried, a fake daemon that never replies to the hello ends the round after one attempt (`FERREX_TEST_PROXY_TIMEOUT_MS`); closing stdin during the accept poll leaves no live child and releases the lock
reconnect
  fails when: after initialize, a failed round → JSON-RPC errors to pending requests, proxy stays up
  proven by:  against the fake daemon: a request in flight at close gets -32603; the next request triggers a reconnect and replay and succeeds without the client re-initialising; lines sent during reconnect arrive in order; reconnecting to a daemon with a different key sends `notifications/tools/list_changed` to the client and a same-key reconnect doesn't; a protocol refusal yields an error response telling the user to /mcp reconnect, not a proxy exit; after a failed round the next request starts a new round that succeeds; a stalled replay ends the round while EOF during replay is retried; client notifications sent while disconnected never reach the reconnected daemon, except `notifications/initialized`; a `ping` in flight when the daemon dies before init gets -32603; daemon closing before the initialize response is retried at once; stdin EOF while a reconnect is pending exits 0; a client that never sends `initialize` (inline `_meta` lifecycle) reconnects after daemon loss without replay and its next request succeeds; in that lifecycle, a daemon that closes before answering the first request causes an immediate reconnect and the client gets that response without re-sending; while that re-sent request is still pending, a later request and a `notifications/cancelled` for it reach the daemon; a daemon that dies again while it's pending gets the kept line re-sent alone, other in-flight ids get -32603, and when it dies a third time while the re-sent line is pending (the fourth daemon loss counting the original), the round's three attempts are used up and the proxy exits non-zero; a later request succeeding while the kept id is pending makes the session initialised, after which a daemon loss gives the kept id -32603 and the proxy doesn't exit. Plus one real-daemon test: kill -9 the daemon mid-session and the next request succeeds
server
  fails when: unchanged from today
  proven by:  two sessions with different namespaces on one daemon each store/recall/timeline in their own namespace; an explicit param wins; the `initialize` result advertises `capabilities.tools.listChanged: true` and `serverInfo` name `ferrex` with the crate version
core-namespace
  fails when: unchanged from today
  proven by:  taxonomy with no scope reports the session namespace's counts and still lists all namespaces; stats diagnostics reports the requested namespace's entity count and `collection`, while `memory_count` stays database-wide
cli
  fails when: unchanged
  proven by:  mcp_protocol test passes with --no-daemon; e2e test passes through the proxy; FERREX_NO_DAEMON=1 selects standalone mode and =0 doesn't; an in-memory --db-path selects standalone mode
sidecar-policy
  fails when: restart fails or the 3-in-10-minutes budget is used up → logged, daemon keeps running and requests error as today; the watcher keeps watching and retries once the window frees
  proven by:  a LeaveRunning sidecar that spawned qdrant leaves it running, and its PID file in place, on drop and on shutdown; KillOnDrop keeps today's behaviour; killing qdrant under a LeaveRunning owner gets it reaped and restarted on the same port; killing qdrant after its owner has exited gets a LeaveRunning reuser to respawn it and take ownership, and two reusers produce one respawn; a `qdrant.pid` naming a live non-qdrant process (e.g. `sleep`) that doesn't answer the probe is treated as stale and a fresh qdrant starts; a live qdrant started through a wrapper script is reused; a `qdrant.port` that differs from the configured port fails at once with the port-mismatch error; a missing `qdrant.port` skips the check; a crashed qdrant left as a zombie under a `KillOnDrop` owner that is still running is treated as dead and respawned by a `LeaveRunning` reuser; `KillOnDrop` shutdown leaves a `qdrant.pid` naming another PID, and its `qdrant.port`, in place; `pid_alive` returns false for an unreaped zombie child (a plain unit test, not `#[ignore]`; it runs inside `nix build`'s check phase, which is why the check avoids a `ps` on `PATH`, and on macOS it skips with a message if `/bin/ps` is missing); `matches_qdrant_name` accepts `qdrant`, `/nix/store/.../qdrant` and `.qdrant-wrapped` (a pure test, no process lookup); `is_qdrant` on a PID whose name can't be read returns false rather than an error. A `start` whose `qdrant_bin` is a fake script that exits at once without listening (`#!/bin/sh` + `exit 1`, so `try_wait` fails the health check on its first poll instead of after the 30 s budget) fails the health check, leaves no live or zombie child, and removes `qdrant.pid` and `qdrant.port` (this one needs no real qdrant, so it isn't `#[ignore]`). The probe-fail re-check that finds a changed PID is a two-starter race, covered by review. A `qdrant.pid` under 30 s old naming a live non-qdrant process gets the full health check rather than one probe (a `sleep` whose PID file is backdated past 30 s still takes the single probe). A fake `qdrant_bin` that exits at once while a real qdrant already answers on the port makes `start` fail rather than succeed. All at the `QdrantSidecar` level, in `#[tokio::test(flavor = "multi_thread")]` since the watcher uses `Handle::block_on`, via its existing `base_dir: Option<PathBuf>` parameter with a temp dir and a free port. Today's `test_sidecar_lifecycle` passes `None` and port 6340; Task 2 migrates it to a temp dir and a free port, and the new tests do the same, so none of them touch the developer's `~/.ferrex`. The `from_config_with` error path is a one-line policy pass-through, covered by review. These need a real qdrant binary, so they're `#[ignore]` like `test_sidecar_lifecycle` and run where qdrant is installed (the dev shell has it)
admin-guard
  fails when: live daemon on the same DB → refusal with instructions, exit non-zero
  proven by:  reconcile --fix refuses when a daemon with a different key uses the same DB; nuke on the default path refuses when a daemon serves a different DB; nuke on a custom --db-path outside ~/.ferrex refuses only for a daemon on that DB, and leaves qdrant running; nuke on the default path with --qdrant-url still stops the sidecar; all succeed when none; a stale PID file whose PID is alive but whose socket doesn't accept doesn't block
```

Prep refactors (needed for this work):
- derive `Serialize` on `FerrexConfig` and every type it contains (crates/core types.rs and
  staleness.rs, crates/embed tiers plus a serde dependency) — blocks daemon-addr's key.

`run_server` in `main.rs` moves to the standalone path, with its `FerrexServer::new` call
replaced by `FerrexServer::with_session(.., SessionDefaults { namespace: config.namespace })`.

## Execution Strategy

Subagents. T1 builds the end-to-end path that everything else extends. After it, T2, T3 and T4
touch (nearly) disjoint files and can run in parallel. All four land in one branch and are
merged together. Shipping T1 alone would break two anti-goals: until T2, every session on a daemon,
including the one that spawned it, uses the `"default"` namespace whatever its
`FERREX_NAMESPACE` (so T1 tests use the default namespace), and until T3, a daemon
crash would leave every session with an exited MCP server.

The spec's T1–T4 are broken into smaller tasks below. T1 became Tasks 1–9, T2 is Task 10, T3 is Tasks 11 and 13, T4 is Tasks 12 and 14. Everything still lands on `feat/shared-daemon` and merges together, for the reason the spec gives.

## Task Dependency Graph

- Task 1 [AFK] (units: Serialize prep refactor, `ensure_cache_dir`): depends on `none` → batch 1
- Task 2 [AFK] (units: sidecar-policy — `pid_alive`, identity and port checks, error-path cleanup): depends on `none` → batch 1 (disjoint: ferrex-store only)
- Task 3 [AFK] (units: handshake): depends on `none` → batch 1 (disjoint: new server files)
- Task 4 [AFK] (units: sidecar-policy — `SidecarPolicy`, watcher, `from_config_with`): depends on `Task 2` → batch 2
- Task 5 [AFK] (units: cli path absolutising, server `with_session`, standalone module): depends on `Task 1` → batch 2 (parallel with Task 4; both edit `pub use` lines in `crates/store/src/lib.rs`: Task 4 adds `SidecarPolicy` on the `sidecar` line, Task 5 adds `is_memory_path` on the `metadata` line. On merge keep both, then `cargo check --workspace`)
- Task 6 [AFK] (units: daemon-addr, `daemon-paths` subcommand): depends on `Task 1, Task 3, Task 5` → batch 3
- Task 7 [AFK] (units: spawn): depends on `Task 3, Task 6` → batch 4
- Task 8 [AFK] (units: daemon, test harness): depends on `Task 3, Task 4, Task 5, Task 6` → batch 4 (parallel with Task 7; both add one `pub mod` line to `crates/server/src/daemon.rs`, and both edit `[dev-dependencies]` in `crates/server/Cargo.toml` — Task 7 adds `test-util` to the tokio features, Task 8 adds `uuid` on the adjacent line; on merge keep both, then `cargo check --workspace`. Only Task 8 changes `Cargo.lock`)
- Task 9 [AFK] (units: proxy, cli routing, fake daemon): depends on `Task 7, Task 8` → batch 5
- Task 10 [AFK] (units: server namespace injection, core-namespace): depends on `Task 9` → batch 6
- Task 11 [AFK] (units: reconnect — initialised sessions): depends on `Task 9` → batch 6
- Task 12 [AFK] (units: daemon-addr `live_for_db`/`any_live`, admin-guard): depends on `Task 9` → batch 6
- Task 13 [AFK] (units: reconnect — pre-init and inline lifecycle): depends on `Task 11` → batch 7
- Task 14 [AFK] (units: `nuke` and qdrant, docs): depends on `Task 10, Task 12` → batch 7 (after Task 10 because both edit `commands.rs`)

In batch 6 only Task 11 may edit `crates/server/tests/common/mod.rs`; Tasks 10 and 12 use what Task 9 left there.

## Agent Assignments

- Task 1: Serialize and cache-dir prep → rust-engineer (Rust)
- Task 2: pid_alive and qdrant identity → rust-engineer (Rust)
- Task 3: handshake → rust-engineer (Rust)
- Task 4: SidecarPolicy and watcher → rust-engineer (Rust)
- Task 5: path absolutising, with_session, standalone → rust-engineer (Rust)
- Task 6: daemon-addr and daemon-paths → rust-engineer (Rust)
- Task 7: spawn and run_round → rust-engineer (Rust)
- Task 8: daemon → rust-engineer (Rust)
- Task 9: proxy happy path → rust-engineer (Rust)
- Task 10: per-session namespace → rust-engineer (Rust)
- Task 11: reconnect for initialised sessions → rust-engineer (Rust)
- Task 12: admin guard → rust-engineer (Rust)
- Task 13: reconnect before init and inline lifecycle → rust-engineer (Rust)
- Task 14: nuke and docs → rust-engineer (Rust + markdown)
- Polish: post-implementation-polish → rust-engineer (all-Rust diff)

## Global Constraints

- Unix only (Linux, macOS). No Windows code paths; drop the existing `#[cfg(not(unix))]` branch when `run_server` moves.
- Rust ≥ 1.89 (`std::fs::File::lock`/`try_lock`); the dev shell has 1.99.
- `just clippy` (`cargo clippy --workspace --all-targets -- -D warnings`, pedantic + nursery) and `just check-fmt` pass at the end of every task. A module not yet reachable from `main` may carry `#[allow(dead_code)]` on its `mod` line; Task 9 removes every such allow. An item that is reachable but has no non-test caller until a later task gets `#[allow(dead_code, reason = "used by Task N")]` (`allow`, not `expect`: an item that unit tests use is live in the `--all-targets` test build, and an unfulfilled `expect` fails `-D warnings`), and Task N removes it. Each task lists the items it expects that way; after removing allows, run `just clippy` and add any field it still reports.
- Source modules use `foo.rs` + `foo/bar.rs`, never `mod.rs`. Integration-test helpers stay in `crates/server/tests/common/mod.rs`, matching `crates/core/tests/common/mod.rs`.
- New workspace deps, exactly: `sha2 = "0.11"` (0.11 reuses the locked `cpufeatures 0.3`; 0.10 would add a duplicate 0.2 that `deny.toml` warns on), `rustix = { version = "1", features = ["process"] }`. ferrex-store and ferrex-server use `rustix.workspace = true`; ferrex-embed gains `serde.workspace = true`; ferrex-server moves `ferrex-store` from dev-dependencies to dependencies.
- `serde_json/preserve_order` must stay off.
- The proxy writes to stdout only whole lines from the daemon, its own synthetic JSON-RPC error responses, and `notifications/tools/list_changed`, all through one writer task. Its diagnostics go to stderr.
- Every proxy exit flushes stdout/stderr and ends in `std::process::exit(code)`. The daemon also ends in `std::process::exit`.
- Run dir: `FERREX_RUN_DIR` or `~/.ferrex/run`, mode 0700. Files: `<key>.sock`, `<key>.pid` (`{"pid":N,"db_path":"/abs"}`), `<key>.lock`, `ferrexd.log`.
- Budgets: flock 300 s; accept poll 30 s; hello reply 3600 s; replay 30 s; forwarded `initialize` re-send 30 s (shares `budgets.replay`); daemon hello read 10 s; init timeout `FERREX_INIT_TIMEOUT` default 1800 s, cap 3000 s; idle `FERREX_IDLE_TIMEOUT` default 600 s; init-failed grace 60 s; shutdown lock wait 10 s; `service.shutdown()` 10 s; round = 3 attempts with 0.5 s then 1 s backoff.
- Test-only env hooks: `FERREX_TEST_DAEMON_EXIT_BEFORE_BIND`, `FERREX_TEST_DAEMON_BIND_DELAY_MS` (added by this plan, see Task 8), `FERREX_TEST_INIT_FAIL_GRACE_MS`, `FERREX_TEST_INIT_DELAY_MS`, `FERREX_TEST_IDLE_PAUSE_MS`, `FERREX_TEST_PROXY_TIMEOUT_MS`. Read only when set.
- Tests never touch the developer's `~/.ferrex`: they pass `--db-path` and `--config-path` explicitly, set `FERREX_RUN_DIR` to a temp dir, and set `HOME` to a temp dir whenever a code path resolves `~/.ferrex` (sidecar dir, `nuke`). Exception: the fastembed model cache. Real-daemon tests inherit `FASTEMBED_CACHE_DIR` (CI sets it; locally it defaults to `~/.ferrex/models`) so models download once, not per test. A test that sets `HOME` to a temp dir and starts a real daemon must pass `FASTEMBED_CACHE_DIR` through explicitly.
- Unit tests that set env vars or change cwd rely on nextest's process-per-test model. Run them with `cargo nextest`, never `cargo test`.
- Server and core integration tests need a `qdrant` binary on `PATH` (the dev shell has one) and run in the `serial-integration` group. Store-level tests that need qdrant are `#[ignore]` like `test_sidecar_lifecycle`; run them with `--run-ignored all`.
- Tasks in one batch that add dependencies (Tasks 1 and 2) both touch `Cargo.lock`; on merge, take either side and run `cargo check --workspace` to regenerate it. Tasks 7 and 8 share `crates/server/Cargo.toml`; see the batch note above.
- Commits: short one-line messages, no `Co-Authored-By` trailer (user's global instruction). Commit after each verified step. `docs/plans/` and `docs/specs/` stay untracked (the repo never commits them), so the `- [x]` flip is an on-disk edit only and is not staged; resume state lives in this working tree.

## Review Focus

1. A single very large line (a multi-MB `recall` result or `store` payload) crossing the proxy. Expected: forwarded intact, no deadlock against a small socket buffer. Test added to Task 9 (`large_line_round_trips`).
2. A client line that isn't JSON, isn't UTF-8 clean, or is a JSON-RPC batch array. Expected: forwarded untracked, proxy keeps running. Test added to Task 9 (tracker unit tests).
3. Ids that differ only in type (`1` vs `"1"`). Expected: tracked as different requests, so one response never clears the other. Test added to Task 9 (`tracker_distinguishes_numeric_and_string_ids`).
4. `FERREX_RUN_DIR` pointing at a regular file or an unwritable directory. Expected: a startup error naming the path, exit non-zero before `initialize`, never a hang. Tests added to Task 6 (`daemon_paths_reports_unusable_run_dir`) and Task 9 (`unusable_run_dir_fails_fast`, the proxy path).
5. A Claude session killed hard (proxy gets SIGKILL, no stdin EOF) while it holds the spawn lock or mid-spawn. Expected: the kernel drops the flock, the next proxy spawns or reuses normally. Test added to Task 9 (`lock_held_by_killed_process_does_not_block`, already a spec item, extended to kill the proxy itself mid-accept-poll).

---

### Task 1: Serialize and cache-dir prep

**Files:**
- Modify: `crates/core/src/types.rs` (`FerrexConfig`, `DedupConfig`, `ConflictConfig`, `PredicatesConfig`, `NamespacePredicatesConfig`, `ReconciliationConfig`, `CacheConfig`)
- Modify: `crates/core/src/staleness.rs` (`StalenessConfig`, `StalenessWeights`, `TypeStalenessConfig`)
- Modify: `crates/embed/src/lib.rs`, `crates/embed/Cargo.toml`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Produces: `impl Serialize for FerrexConfig` (and every contained type); `pub fn ferrex_core::ensure_cache_dir()`.

- [x] **Step 1: Write the failing test** in a new `#[cfg(test)] mod tests` in `crates/core/src/types.rs` (it has none yet)

```rust
#[test]
fn ferrex_config_serializes_with_sorted_keys() {
    let value = serde_json::to_value(sample_config()).unwrap();
    let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    assert_eq!(value["model_tier"], "Small");
    assert!(value["staleness"]["type_config"].get("episodic").is_some());
}
```

Add a `sample_config()` helper in that module. `FerrexConfig` has no `Default`, so build it as a struct literal (copy the field list from `crates/core/tests/common/mod.rs` or `crates/server/src/cli.rs`): `ModelTier::Small`, `config_path: None`, `StalenessConfig::default()`, `::default()` for the dedup/conflict/reconciliation/cache sub-configs, a `PredicatesConfig` with two groups, and literals for the rest.

- [x] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p ferrex-core ferrex_config_serializes_with_sorted_keys`
Expected: compile error, `Serialize` not implemented for `FerrexConfig`.

- [x] **Step 3: Derive `Serialize`** on every listed type; add `serde = { workspace = true }` to ferrex-embed and derive it on `ModelTier` and `RerankerTier` (default representation, variant names). Make `ensure_cache_dir` `pub` and add it to the `pub use ferrex_embed::{...}` line in `crates/core/src/lib.rs`. Change its SAFETY comment to say `main` calls it before any thread starts; leave the existing calls in `Embedder::new`/`Reranker::new` (the `Once` makes them no-ops).

- [x] **Step 4: Run tests**

Run: `cargo nextest run -p ferrex-core -p ferrex-embed --lib && just clippy`
Expected: PASS, no warnings.

---

### Task 2: pid_alive and qdrant identity

**Files:**
- Modify: `Cargo.toml` (workspace `rustix`), `crates/store/Cargo.toml` (`rustix`; dev-dependencies `tempfile`, `portpicker`)
- Modify: `crates/store/src/sidecar.rs`, `crates/store/src/lib.rs`

**Interfaces:**
- Produces (re-exported from `ferrex_store`):
  - `pub fn pid_alive(pid: u32) -> bool`
  - `pub fn is_qdrant(pid: u32, qdrant_bin: &str) -> bool`
  - `pub async fn probe_qdrant(port: u16) -> bool`
- Public (re-exported, for Task 14's `nuke`): `pub fn read_pid(path: &Path) -> Option<u32>` (today's private helper), `pub fn read_port_file(path: &Path) -> Option<(u32, u16)>`.
- Private: `fn matches_qdrant_name(comm: &str, qdrant_bin: &str) -> bool`, `struct SidecarLock { path: PathBuf, _file: File }` returned by `acquire_lock_file`, whose `Drop` removes the lock file (today the `create_new` lock file is deleted only on the two success paths, so any early `?` return leaves it and blocks every `start` for 30 s), `fn remove_pid_files_if_ours(dir: &Path, pid: u32)` (takes its own `SidecarLock`, removes `qdrant.pid` and `qdrant.port` only if `qdrant.pid` names `pid`, leaves both if the lock can't be taken), `async fn health_check(port: u16, child: Option<&mut Child>) -> Result<(), StoreError>`.
- `QdrantSidecar::start` keeps its three-argument signature in this task.

- [x] **Step 1: Write the failing unit tests** in `sidecar.rs`

- `start_error_under_lock_releases_it` (no qdrant needed): `start` with port 1 errors under the lock; an immediate second `start` doesn't fail with the lock-timeout message.
- `pid_alive_true_for_self`: `assert!(pid_alive(std::process::id()))`.
- `pid_alive_rejects_zero_and_out_of_range`: `!pid_alive(0)`, `!pid_alive(u32::MAX)`.
- `pid_alive_false_for_unreaped_zombie`: spawn `/bin/sh -c "exit 0"` with `std::process::Command`, sleep 300 ms without `wait`, `assert!(!pid_alive(child.id()))`, then `child.wait()`. On macOS, return early with an `eprintln!` if `/bin/ps` is missing.
- `matches_qdrant_name_accepts_variants`: with `qdrant_bin = "qdrant"` and with `"/nix/store/abc/bin/qdrant"`, accepts `"qdrant"`, `"/nix/store/xyz-qdrant-1.13/bin/qdrant"`, `".qdrant-wrapped"`; rejects `"sh"`, `"sleep"`, `"qdrantx-other"`.
- `is_qdrant_false_when_name_unreadable`: `!is_qdrant(u32::MAX - 1, "qdrant")`.
- `start_fails_and_cleans_up_when_bin_exits_immediately`: write `#!/bin/sh\necho $$ > "$(dirname "$0")/child.pid"\nexit 1\n` to a temp file with mode 0755; `QdrantSidecar::start(script, free_port, Some(dir)).await` is `Err` well under 30 s (assert elapsed < 10 s); `qdrant.pid` and `qdrant.port` don't exist; and right after `start` returns, `rustix::process::test_kill_process` on the PID from `child.pid` fails with `ESRCH` (an unreaped zombie would still succeed here, while `pid_alive` reports zombies as dead, so don't use `pid_alive` for this check). This test runs under multithreaded `cargo test` in nix's check phase, where another thread forking while the script is still open for writing makes the exec fail with ETXTBSY (rust-lang/rust#114554); retry the whole `start` call up to 3 times while it returns `Err(StoreError::Sidecar(msg))` with `msg` containing `format!("os error {}", rustix::io::Errno::TXTBSY.raw_os_error())` (`start` stringifies the spawn `io::Error`, so no `ErrorKind` is available).

- [x] **Step 2: Write the `#[ignore = "requires qdrant binary on PATH"]` tests** (temp `base_dir`, `portpicker` port, never `None`/6340)

- `port_file_mismatch_fails_fast`: start qdrant on port A (`qdrant.port` now reads `<pid> <A>`), then `start(.., port B, ..)` in the same dir fails in < 5 s with a message containing `is running on port` and `but qdrant_port is`.
- `missing_port_file_skips_check`: delete `qdrant.port`; a second `start` on the same port reuses (same `pid()`).
- `stale_pid_of_non_qdrant_process_is_replaced`: spawn `sleep 60`, write its PID to `qdrant.pid`, backdate the file 60 s with `File::set_modified`; `start` succeeds with a new PID.
- `fresh_pid_file_of_non_qdrant_gets_full_health_check`: same without backdating; assert elapsed ≥ 25 s before `start` returns Ok with a new PID.
- `wrapper_script_qdrant_is_reused`: start qdrant through `#!/bin/sh\nexec qdrant "$@"` and again through a non-exec wrapper `#!/bin/sh\nqdrant "$@" &\necho $! > "$(dirname "$0")/inner.pid"\nwait`; a second `start` with bin `"qdrant"` reuses the same PID. `KillOnDrop` only SIGKILLs the direct child (`sh`), so the non-exec case holds a drop guard that reads `inner.pid` and SIGKILLs it with `rustix::process::kill_process`, so the orphaned qdrant is reaped even when an assert panics.
- `killondrop_shutdown_leaves_foreign_pid_file`: start, overwrite `qdrant.pid` with another live PID and `qdrant.port` with `<that pid> <port>`, `shutdown()`; both files still exist with that content.
- `bin_exits_while_real_qdrant_answers_fails`: real qdrant on port P in dir D1; in dir D2, `start(exit-1 script, P, Some(D2))` returns `Err`.
- Migrate the existing `test_sidecar_lifecycle` in `sidecar.rs` from `start("qdrant", 6340, None)` to a `tempfile::tempdir()` base dir and a `portpicker` port; assert the url contains that port and keep the `!pid_file.exists()` check. Otherwise the `sidecar` filter below runs it against the developer's `~/.ferrex`, where a live ferrex sidecar fails it on the port check.

- [x] **Step 3: Run them to verify they fail**

Run: `cargo nextest run -p ferrex-store --lib --run-ignored all sidecar`
Expected: compile errors for the missing functions.

- [x] **Step 4: Implement**

- `pid_alive`: `i32::try_from(pid)` then `Pid::from_raw`; `None`/error → false. `rustix::process::test_kill_process(pid)`: `Ok` → zombie check, `EPERM` → true, `ESRCH`/other → false. Zombie check: Linux reads `/proc/<pid>/stat`, state is the first field after the last `)`, `Z` → dead; macOS runs `/bin/ps -o stat= -p <pid>`, leading `Z` → dead. If the check can't run or returns nothing, the `kill` result stands. Replace every `is_process_alive` use.
- Process name: Linux `/proc/<pid>/comm`; macOS `/bin/ps -o comm= -p <pid>`. Unreadable/empty → `None` → `is_qdrant` false.
- `matches_qdrant_name`: basename of both, strip leading `.` and trailing `-wrapped` from the process name, compare the first 15 chars.
- `probe_qdrant`: one `health_check` call, client built with `.timeout(5 s).skip_compatibility_check()`, no retries.
- `health_check`: client gets `.skip_compatibility_check()`. When `child` is `Some`, sleep `HEALTH_CHECK_INITIAL_DELAY` before the first poll, call `try_wait()` before each poll and once more after a successful response, and fail with "qdrant exited during health check" if it exited. The post-success check is what keeps a child that dies while someone else's qdrant answers from counting as healthy.
- After spawn: write `qdrant.pid`, then `qdrant.port` as `"<pid> <port>"`, then release the sidecar lock by dropping the `SidecarLock`. Every return between acquiring and releasing the lock (port check, `create_dir_all`, config write, `spawn()` error such as ETXTBSY, PID/port write) deletes the file through the guard; the reuse-path re-take uses the same guard. `start` drops its guard before calling `remove_pid_files_if_ours` on any path, including a failed `qdrant.port` write. Test: Step 1's `start_error_under_lock_releases_it`.
- Reuse path per spec §"Qdrant sidecar in daemon mode" (identity check, port check, probe outside the lock, full health check when `qdrant.pid` mtime < `HEALTH_CHECK_MAX_TIMEOUT`, re-take lock and re-read on probe failure). Every removal of `qdrant.pid` also removes `qdrant.port`.
- Spawn error path: kill and `wait` the child, then `remove_pid_files_if_ours`, then return the error.
- `shutdown()`: kill, `wait`, then `remove_pid_files_if_ours`.
- `lib.rs`: `pub use sidecar::{QdrantSidecar, is_qdrant, pid_alive, probe_qdrant, read_pid, read_port_file};`

Port-mismatch message, verbatim: `qdrant sidecar (pid {pid}) is running on port {recorded}, but qdrant_port is {port}. Set qdrant_port = {recorded}, or close the sessions still using the old config (or kill their ferrex daemons), then stop qdrant`

- [x] **Step 5: Run tests**

Run: `cargo nextest run -p ferrex-store --lib --run-ignored all sidecar && just clippy`
Expected: PASS.

---

### Task 3: Handshake

**Files:**
- Create: `crates/server/src/daemon.rs` (`pub mod handshake;` only, for now)
- Create: `crates/server/src/daemon/handshake.rs`
- Modify: `crates/server/src/main.rs` (`#[allow(dead_code)] mod daemon;`)

**Interfaces:**
- Produces:

```rust
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub ferrex_hello: u32,
    pub key: String,
    pub namespace: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefusalKind { Protocol, Key, Init, Session }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<RefusalKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
```

- `Hello::new(key: String, namespace: String) -> Self`
- `Hello::encode(&self) -> String` and `HelloReply::encode(&self) -> String` (JSON plus trailing `\n`)
- `Hello::decode(line: &str) -> Result<Self, HelloReply>`: not JSON or missing fields → `Protocol`; `ferrex_hello != PROTOCOL_VERSION` → `Protocol` with both versions in the message. Read `ferrex_hello` from a `serde_json::Value` first so an unknown version with other fields still gets the version message.
- `Hello::check(&self, key: &str) -> Result<(), HelloReply>`: mismatch → `Key`, message names both keys.
- `HelloReply::ok() -> Self`, `HelloReply::refuse(kind: RefusalKind, error: impl Into<String>) -> Self`, `HelloReply::decode(line: &str) -> Result<Self, String>`.

- [x] **Step 1: Write the failing unit tests** in `handshake.rs`

- `hello_round_trips`: `Hello::decode(&Hello::new("k".into(), "ns".into()).encode())` equals the original.
- `ok_reply_encodes_exactly`: `HelloReply::ok().encode() == "{\"ok\":true}\n"`.
- `refusal_encodes_kind_lowercase`: `refuse(Init, "boom").encode()` contains `"kind":"init"` and `"error":"boom"`.
- `version_mismatch_is_protocol`: decode `{"ferrex_hello":2,"key":"k","namespace":"n"}` → `Err` with `kind == Some(Protocol)`.
- `malformed_line_is_protocol`: decode `not json` → `Err(Protocol)`.
- `key_mismatch_is_key`: `check("other")` → `Err` with `kind == Some(Key)`, error contains both keys.

- [x] **Step 2: Run to verify failure**

Run: `cargo nextest run -p ferrex-server handshake`
Expected: compile errors.

- [x] **Step 3: Implement** the types and functions above.

- [x] **Step 4: Run tests**

Run: `cargo nextest run -p ferrex-server handshake && just clippy`
Expected: PASS.

---

### Task 4: SidecarPolicy and watcher

**Files:**
- Modify: `crates/store/src/sidecar.rs`, `crates/store/src/lib.rs`
- Modify: `crates/core/src/service.rs`, `crates/core/src/lib.rs`
- Modify: call sites `crates/core/tests/common/mod.rs`, `crates/server/tests/mcp_protocol.rs`, the `test_sidecar_lifecycle` test

**Interfaces:**
- Consumes: Task 2's `pid_alive`, `is_qdrant`, `remove_pid_files_if_ours`, `health_check`.
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum SidecarPolicy { KillOnDrop, LeaveRunning }`, re-exported from `ferrex_store` and `ferrex_core`.
  - `pub async fn QdrantSidecar::start(bin: &str, port: u16, base_dir: Option<PathBuf>, policy: SidecarPolicy) -> Result<Self, StoreError>`
  - `pub async fn MemoryService::from_config_with(config: FerrexConfig, policy: SidecarPolicy) -> Result<Self, CoreError>`; `from_config(config)` calls `from_config_with(config, SidecarPolicy::KillOnDrop).await`.
- `#[cfg(test)] pub(crate) fn stop_watcher(&self)`: sets an `Arc<AtomicBool>` the watcher checks after each `wait()` return and each poll; once set, the watcher exits without restarting (and without killing a `Child` it owns). This is how tests simulate "owner gone" in one process.
- Private: `struct Started { pid: u32, child: Option<Child> }`, `async fn start_inner(ctx: &StartCtx) -> Result<Started, StoreError>` (today's start logic, no watcher), `struct RestartBudget` with `fn available(&mut self, now: Instant) -> bool`, `fn spend(&mut self, now: Instant)`; consts `RESTART_LIMIT = 3`, `RESTART_WINDOW = 600 s`, `REUSE_POLL = 2 s`.

- [x] **Step 1: Write the failing unit tests** in `sidecar.rs`

- `restart_budget_allows_three_per_window`: three `spend(t0)`; `available(t0 + 1 s)` is false; `available(t0 + 601 s)` is true.
- `restart_budget_frees_oldest_first`: spends at t0, t0+5 min, t0+6 min; `available(t0 + 10 min + 1 s)` is true and only one slot is free.

`#[ignore = "requires qdrant binary on PATH"]`, `#[tokio::test(flavor = "multi_thread")]`, temp `base_dir` and free port. Every `LeaveRunning` test holds a cleanup guard that runs on scope exit, including on panic: it calls `stop_watcher()` on every still-live sidecar (before the kill, or the watcher respawns qdrant), reads the *current* PID from `qdrant.pid` (a respawn changes it), SIGKILLs it if it is > 1 and `is_qdrant` confirms it, and polls up to 2 s for `!pid_alive`. The guard logs failures instead of asserting (a panic during unwinding aborts); test bodies assert `pid_alive` explicitly where it matters.
- `leave_running_keeps_qdrant_on_drop_and_shutdown`: start `LeaveRunning`, record pid, `stop_watcher()`, `shutdown()` then drop; `pid_alive(pid)` and `qdrant.pid` still exist. The guard kills it at the end.
- `kill_on_drop_unchanged`: start `KillOnDrop`, drop; within 2 s `!pid_alive(pid)` and no `qdrant.pid`.
- `killed_qdrant_restarted_by_owner_on_same_port`: `LeaveRunning`, SIGKILL the qdrant pid; within 15 s `probe_qdrant(port)` is true and `qdrant.pid` names a new live pid.
- `reuser_respawns_after_owner_gone_and_takes_ownership`: sidecar A spawns (`LeaveRunning`), sidecar B reuses; `A.stop_watcher()`, drop A, SIGKILL qdrant; within 15 s `qdrant.pid` names a new live pid spawned by B (B's watcher now holds a `Child`: the guard stops B's watcher and kills the pid, and the test body checks it is reaped, i.e. `pid_alive` false within 2 s).
- `two_reusers_single_respawn`: as above with B and C, A's watcher stopped; after respawn `qdrant.pid` names one live pid and only one qdrant process listens (second `start` reuses it).
- `zombie_under_killondrop_owner_respawned_by_reuser`: A `KillOnDrop` spawns, B `LeaveRunning` reuses; SIGKILL qdrant without A reaping it; B respawns within 15 s.

- [x] **Step 2: Run to verify failure**

Run: `cargo nextest run -p ferrex-store --lib --run-ignored all sidecar`
Expected: compile errors.

- [x] **Step 3: Implement**

- `start` runs `start_inner`; under `LeaveRunning` it captures `Handle::current()` and spawns a `std::thread` watcher. If it owns a `Child` the watcher calls `wait()`; if it reused, it polls `pid_alive` every `REUSE_POLL`. When qdrant is gone and budget allows, it calls `handle.block_on(start_inner(&ctx))`; a run that spawns or fails spends one unit, a pure reuse spends none. When it spawns it switches to `wait()`, when it reuses it switches to polling. Out of budget: log once, keep watching, retry when the oldest unit is older than `RESTART_WINDOW`. Log each restart with `tracing::warn!`.
- `LeaveRunning`: `shutdown()` and `Drop` do nothing to the process or `qdrant.pid`/`qdrant.port`. `KillOnDrop` keeps Task 2's behaviour.
- `MemoryService::from_config_with` passes `policy` to `QdrantSidecar::start`.
- Existing call sites pass `SidecarPolicy::KillOnDrop`.

- [x] **Step 4: Run tests**

Run: `cargo nextest run -p ferrex-store --lib --run-ignored all sidecar && cargo nextest run -p ferrex-core && just clippy`
Expected: PASS.

---

### Task 5: Path absolutising, with_session, standalone

**Files:**
- Modify: `crates/server/src/cli.rs`, `crates/server/src/server.rs`, `crates/server/src/main.rs`, `crates/server/Cargo.toml` (move `ferrex-store` from `[dev-dependencies]` to `[dependencies]`; `build_config` calls `ferrex_store::is_memory_path`), `crates/store/src/metadata.rs`, `crates/store/src/lib.rs`
- Create: `crates/server/src/standalone.rs`

**Interfaces:**
- Consumes: Task 1's `ferrex_core::ensure_cache_dir`.
- Produces:
  - `pub fn ferrex_store::is_memory_path(path: &Path) -> bool` (`:memory:` literal or any path containing `mode=memory`), extracted from the inline check in `SqliteStore::open_with_pool_size` (`metadata.rs:362`), which now calls it; `build_config`, `use_standalone` and `admin_guard` call it too, so the CLI and store can't drift
  - `fn cli::absolutize(path: &Path) -> eyre::Result<PathBuf>` (`create_dir_all(parent)`, empty parent → cwd, `canonicalize(parent)`, join file name)
  - `build_config` returns absolute `db_path` and `config_path` (`config_path` absolutised after `load_or_init`); in-memory `db_path` passes through untouched.
  - `#[derive(Debug, Clone)] pub struct SessionDefaults { pub namespace: String }` in `server.rs`
  - `FerrexServer::with_session(service: Arc<MemoryService>, defaults: SessionDefaults) -> Self`, replacing `FerrexServer::new`. Stores `defaults`; after building `Self`, logs `tracing::info!(namespace = %server.defaults.namespace, "session started")` from the field, so the field is read and clippy stays quiet.
  - `pub fn standalone::run(config: FerrexConfig) -> eyre::Result<()>`: today's `run_server`, building the server with `with_session(.., SessionDefaults { namespace: config.namespace.clone() })`.

- [x] **Step 1: Write the failing unit tests** in `cli.rs`. Each passes `--config-path` and `--db-path` explicitly via `Cli::try_parse_from`.

- `build_config_absolutizes_relative_db_path`: `set_current_dir(tmp)`, `--db-path sub/x.db` → `db_path == canonicalize(tmp)/sub/x.db` and `tmp/sub` exists.
- `build_config_same_key_path_for_different_spellings`: `--db-path ./a/../b/x.db` and `--db-path <abs tmp>/b/x.db` give equal `db_path`.
- `build_config_keeps_memory_db_path`: `--db-path :memory:` → `db_path == Path::new(":memory:")`, no `:memory:` dir created.
- `build_config_absolutizes_config_path`: relative `--config-path c/ferrex.toml` → absolute, and the file exists.
- `is_memory_path_detects_both_forms` (in `ferrex-store`'s `metadata.rs` tests): true for `:memory:` and `file:x?mode=memory&cache=shared`; false for `/tmp/x.db`.

- [x] **Step 2: Run to verify failure**

Run: `cargo nextest run -p ferrex-server cli:: && cargo nextest run -p ferrex-store --lib is_memory_path`
Expected: FAIL / compile errors.

- [x] **Step 3: Implement** the above. In `main`: call `ferrex_core::ensure_cache_dir()` as the first statement after `color_eyre::install()`, before `Cli::parse()` and `init_tracing()`. In `init_tracing`'s `(None, false)` branch add `.with_ansi(std::io::stderr().is_terminal())`. `None =>` routes to `standalone::run` for now.

- [x] **Step 4: Run tests**

Run: `cargo nextest run -p ferrex-server cli:: && cargo nextest run -p ferrex-store --lib is_memory_path && cargo nextest run -p ferrex-server --test mcp_protocol && just clippy`
Expected: PASS; `mcp_protocol` unchanged and green.

---

### Task 6: daemon-addr and `daemon-paths`

**Files:**
- Modify: `Cargo.toml` (workspace `sha2`), `crates/server/Cargo.toml` (`sha2`, `rustix`)
- Create: `crates/server/src/daemon/addr.rs`; modify `crates/server/src/daemon.rs` (`pub mod addr;`)
- Modify: `crates/server/src/cli.rs` (hidden `Command::DaemonPaths`), `crates/server/src/main.rs`
- Test: `crates/server/tests/daemon_paths.rs`

**Interfaces:**
- Consumes: `FerrexConfig: Serialize` (Task 1), absolute paths from `build_config` (Task 5).
- Produces:

```rust
pub struct DaemonPaths {
    pub run_dir: PathBuf,
    pub key: String,
    pub socket: PathBuf,
    pub pid_file: PathBuf,
    pub lock_file: PathBuf,
    pub log_file: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidFile {
    pub pid: u32,
    pub db_path: PathBuf,
}
```

  - `pub fn run_dir() -> PathBuf` (`FERREX_RUN_DIR`, else `~/.ferrex/run`)
  - `pub fn ensure_run_dir(dir: &Path) -> eyre::Result<()>` (create with 0700; `chmod 0700` if it exists; error names the path)
  - `pub fn compute_key(config: &FerrexConfig, exe: &Path) -> eyre::Result<String>`
  - `DaemonPaths::for_config(config: &FerrexConfig, exe: &Path) -> eyre::Result<Self>` (computes the key, does not create the dir, errors when `socket` exceeds `MAX_SUN_PATH`)
  - `const MAX_SUN_PATH: usize` = 103 on macOS, 107 on Linux (`#[cfg(target_os)]`)
  - `PidFile::write(&self, path: &Path) -> io::Result<()>` (write `<path>.tmp`, then rename), `PidFile::read(path: &Path) -> Option<Self>` (debug-logs and returns `None` on read/parse failure)
  - `pub fn current_exe_canonical() -> eyre::Result<PathBuf>`
  - `ferrex daemon-paths` prints `{"key":..,"socket":..,"pid_file":..,"lock_file":..,"run_dir":..}` on one line.

Key algorithm (the one non-obvious body):

```rust
pub fn compute_key(config: &FerrexConfig, exe: &Path) -> eyre::Result<String> {
    let mtime = std::fs::metadata(exe)?.modified()?.duration_since(UNIX_EPOCH)?.as_nanos();
    let mut value = serde_json::to_value(config)?;
    let obj = value.as_object_mut().ok_or_else(|| eyre::eyre!("config is not an object"))?;
    obj.remove("namespace");
    if config.qdrant_url.is_none()
        && let Some(resolved) = resolve_on_path(&config.qdrant_bin)
    {
        obj.insert("qdrant_bin".into(), serde_json::Value::String(resolved.display().to_string()));
    }
    let mut hasher = Sha256::new();
    hasher.update(exe.as_os_str().as_bytes());
    hasher.update([0]);
    hasher.update(mtime.to_le_bytes());
    hasher.update(serde_json::to_vec(&value)?);
    Ok(hex16(&hasher.finalize()))
}
```

Imports: `sha2::{Digest, Sha256}`, `std::os::unix::ffi::OsStrExt`, `std::time::UNIX_EPOCH`. `fn hex16(d: &[u8]) -> String` (takes the hybrid-array digest output by deref) = lowercase hex of the first 8 bytes of the digest (16 chars); the length keeps `<run_dir>/<key>.sock` under `MAX_SUN_PATH` (see `macos_tempdir_socket_fits`).

`resolve_on_path(bin)`: a `bin` containing `/` is canonicalised as is; otherwise scan `PATH` entries for an executable file and canonicalise the first hit; `None` if not found.

- [x] **Step 1: Write the failing unit tests** in `addr.rs`

- `key_covers_every_field`: an exhaustive destructure with no `..`, so a new field breaks the build:

```rust
let FerrexConfig {
    qdrant_url: _, qdrant_bin: _, qdrant_port: _, model_tier: _, reranker_tier: _,
    namespace: _, db_path: _, config_path: _, deduplication: _, conflict: _,
    predicates: _, reconciliation: _, staleness: _, reader_pool_size: _, cache: _,
} = base_config();
```

- `each_field_changes_key`: a table of `(name, fn(&mut FerrexConfig))` mutations, one per field except `namespace` (including a nested one for `staleness.weights.age`, `predicates.groups`, `cache.result_capacity`); each produces a key different from the base.
- `namespace_does_not_change_key`.
- `same_inputs_same_key`.
- `exe_mtime_changes_key`: temp file as `exe`, key, `File::set_modified(+1 s)`, key differs.
- `qdrant_url_hashes_raw_bin`: with `qdrant_url = Some(..)` and `qdrant_bin = "definitely-not-on-path"`, `compute_key` is `Ok`.
- `socket_path_too_long_errors`: `FERREX_RUN_DIR` nested to make `<run_dir>/<key>.sock` > 107 bytes → `Err` whose message contains the socket path.
- `macos_tempdir_socket_fits`: a run dir of 64 bytes → `Ok`.
- `key_is_16_lowercase_hex`: `compute_key` output has length 16 and only `[0-9a-f]`.
- `pid_file_round_trips` and `pid_file_unparseable_reads_none`.
- `ensure_run_dir_sets_0700`: pre-create with 0755, call, mode is 0700.

- [x] **Step 2: Write the failing integration tests** in `crates/server/tests/daemon_paths.rs` (runs the binary via `NEXTEST_BIN_EXE_ferrex`/`CARGO_BIN_EXE_ferrex`)

- `daemon_paths_same_key_across_cwds_and_spellings`: from cwd A with `--db-path <abs>/x.db` and from cwd B with a relative spelling of the same path → same `key`.
- `daemon_paths_stable_across_processes_with_predicate_groups`: `ferrex.toml` with three predicate groups and a namespace override; two runs print the same key.
- `daemon_paths_reports_unusable_run_dir` (Review Focus 4): `FERREX_RUN_DIR` set to a regular file → non-zero exit, stderr contains that path.

`daemon-paths` calls `ensure_run_dir` so that case surfaces.

- [x] **Step 3: Run to verify failure**

Run: `cargo nextest run -p ferrex-server addr:: && cargo nextest run -p ferrex-server --test daemon_paths`
Expected: compile errors.

- [x] **Step 4: Implement** the interfaces above and wire `Command::DaemonPaths` (`#[command(hide = true)]`) in `main.rs`.

- [x] **Step 5: Run tests**

Run: `cargo nextest run -p ferrex-server addr:: && cargo nextest run -p ferrex-server --test daemon_paths && just clippy`
Expected: PASS.

---

### Task 7: spawn and run_round

**Files:**
- Create: `crates/server/src/daemon/spawn.rs`; add `pub mod spawn;` to `crates/server/src/daemon.rs`
- Modify: `crates/server/Cargo.toml` (add `"test-util"` to the tokio dev-dependency features, needed for `start_paused`)

**Interfaces:**
- Consumes: `DaemonPaths`, `ensure_run_dir` (Task 6); `Hello`, `HelloReply`, `RefusalKind` (Task 3).
- Produces:

```rust
#[derive(Debug, Clone)]
pub struct Budgets { pub flock: Duration, pub accept: Duration, pub hello: Duration, pub replay: Duration }

#[derive(Debug)]
pub struct AttemptError {
    pub kind: Option<RefusalKind>,
    pub retryable: bool,
    pub child_exited_early: bool,
    pub message: String,
}

pub struct Resolved { pub config: FerrexConfig, pub paths: DaemonPaths }
pub struct Connected<T> { pub value: T, pub attempts_used: u8 }
#[derive(Debug)]
pub struct RoundError { pub last: AttemptError, pub any_child_exited_early: bool }

pub struct DaemonConn { pub reader: BufReader<OwnedReadHalf>, pub writer: OwnedWriteHalf, pub key: String }
```

  - `Budgets::from_env() -> Self`: defaults 300 s / 30 s / 3600 s / 30 s; `FERREX_TEST_PROXY_TIMEOUT_MS=<n>` sets all four to `n` ms.
  - `pub async fn connect_or_spawn(paths: &DaemonPaths, exe: &Path, argv: &[OsString], budgets: &Budgets) -> Result<UnixStream, AttemptError>`
  - `pub async fn send_hello(stream: UnixStream, hello: &Hello, timeout: Duration) -> Result<DaemonConn, AttemptError>`: EOF/error before the reply → retryable; timeout on an open connection → not retryable; `ok:false` with `Protocol`/`Init` → not retryable; `Key`/`Session` → retryable.
  - `pub async fn run_round<T, R, A, Fut>(resolve: R, attempt: A, used: u8) -> Result<Connected<T>, RoundError>` (precondition `used < MAX_ATTEMPTS`, `debug_assert!`ed; callers check exhaustion themselves). `Connected::attempts_used` counts `used` plus the attempts made in this call. `RoundError` carries no count, since no caller reads one: a failed round is reported the same whether it exhausted `MAX_ATTEMPTS` or hit a non-retryable error. where `R: FnMut() -> Result<Resolved, AttemptError>`, `A: FnMut(Resolved) -> Fut`, `Fut: Future<Output = Result<T, AttemptError>>`. `MAX_ATTEMPTS = 3`, `BACKOFF = [500 ms, 1 s]`. When `used > 0`, sleep `BACKOFF[used - 1]` before the first attempt, since attempt `used` already failed (spec §"Failure counting": the backoff carries across re-sends; "reconnect at once" only means not waiting for a client request). A non-retryable error ends the round at once. An `Err` from `resolve` counts as a failed attempt and is built as `AttemptError { kind: None, retryable: true, child_exited_early: false, .. }` (spec §"Failure counting": key not computable), so a `ferrex.toml` caught mid-write doesn't end the round.
  - `fn classify_connect_error(e: &io::Error) -> ConnectClass` with `enum ConnectClass { Stale, Busy, Other }`: `NotFound`/`ConnectionRefused`, or `raw_os_error() == Some(rustix::io::Errno::NOTSOCK.raw_os_error())` (macOS returns `ENOTSOCK` for a regular file at the path, Linux `ECONNREFUSED`) → `Stale`; `WouldBlock`, `Interrupted`, `TimedOut` → `Busy`; else `Other`.

`connect_or_spawn` follows spec §"Start-up" step 2 exactly. Notes the spec leaves to the builder:
- Lock: open `lock_file` with `create(true).write(true)`, poll `try_lock()` every 50 ms up to `budgets.flock`; timeout → non-retryable `"timed out waiting for spawn lock <path>"`.
- Run dir: an `ensure_run_dir` failure (path is a regular file, unwritable, wrong owner) → non-retryable, message naming the path. Retrying can't fix it.
- Spawn: `tokio::process::Command::new(exe).args(argv).args(["daemon", "--key", key])`, stdin/stdout null, stderr `OpenOptions::new().append(true).create(true)` on `log_file`, `pre_exec` calling `rustix::process::setsid()`. No `kill_on_drop`.
- `SpawnGuard { child: Option<Child>, lock: Option<File> }`: `Drop` calls `start_kill()` and moves the child into `tokio::spawn(async move { let _ = child.wait().await; })`, then unlocks. `disarm(self) -> Child` on success, after which the child goes to a detached `wait()` task and the lock is released before the hello.
- Accept poll every 100 ms: connect first, then `try_wait()`. Child exited and connect failed → retryable, `child_exited_early: true`, message built by `fn log_hint(log_file: &Path, env_log: Option<&OsStr>) -> String`, which names `log_file` and also `$FERREX_LOG_FILE` (as given) when set, since tracing output goes there (spec §"Daemon", logging bullet). Tests: Step 1's `log_hint_*`.
- Accept budget runs out → the guard kills the child before releasing the lock; retryable (spec §"Failure counting": not in the round-ending list), message from `log_hint`. Test: Task 9's `accept_timeout_kills_child_and_names_log`.

- [x] **Step 1: Write the failing unit tests** in `spawn.rs` (`#[tokio::test(start_paused = true)]` where timing matters)

- `log_hint_names_log_file` (var unset: names only `log_file`) and `log_hint_includes_env_log_file` (var set: names both).
- `run_round_reresolves_key_between_attempts`: `resolve` returns key A then key B; `attempt` refuses A with `kind: Some(Key), retryable: true` and returns `Ok(())` for B; result `attempts_used == 2`.
- `run_round_retries_resolve_error`: `resolve` returns `Err` once, then `Ok`; `attempt` succeeds; `attempts_used == 2`.
- `run_round_stops_after_three_attempts_with_backoff`: always-retryable failure → `Err` after exactly 3 `attempt` calls; virtual time advanced ≥ 1.5 s.
- `run_round_ends_on_non_retryable`: first attempt `kind: Some(Init), retryable: false` → `Err` after one call.
- `run_round_honours_used`: `used = 2`, always failing → exactly one call, then `Err`; `used = 1` with a succeeding attempt → `Connected.attempts_used == 2`. With paused time, the first `attempt` call happens only after virtual time advanced ≥ 500 ms for `used = 1` and ≥ 1 s for `used = 2`.
- `classify_connect_error_maps_kinds`: one assertion per `io::ErrorKind` listed above, plus `io::Error::from_raw_os_error(ENOTSOCK)` → `Stale`.
- `budgets_from_env_test_override`: with `FERREX_TEST_PROXY_TIMEOUT_MS=250`, all four are 250 ms.
- `send_hello_timeout_is_not_retryable` and `send_hello_eof_is_retryable`: against a `UnixStream::pair()` peer that stays silent or drops.

- [x] **Step 2: Run to verify failure**

Run: `cargo nextest run -p ferrex-server spawn::`
Expected: compile errors.

- [x] **Step 3: Implement** the interfaces above.

- [x] **Step 4: Run tests**

Run: `cargo nextest run -p ferrex-server spawn:: && just clippy`
Expected: PASS.

---

### Task 8: Daemon

**Files:**
- Modify: `crates/server/src/daemon.rs` (accept loop, init, idle, shutdown: `pub fn run`)
- Create: `crates/server/src/daemon/session.rs` (per-connection hello, `CollectionMemo`, rmcp serve)
- Create: `crates/server/src/daemon/idle.rs` (`IdleState`), `crates/server/src/daemon/env.rs` (`env_duration`)
- Modify: `crates/server/src/cli.rs` (hidden `Command::Daemon { #[arg(long)] key: String }`), `crates/server/src/main.rs`, `crates/server/Cargo.toml` (dev-dependency `uuid`)
- Create: `crates/server/tests/common/mod.rs`, `crates/server/tests/daemon_lifecycle.rs`

**Interfaces:**
- Consumes: `DaemonPaths`, `PidFile`, `ensure_run_dir`, `compute_key`, `current_exe_canonical` (Task 6); handshake (Task 3); `MemoryService::from_config_with`, `SidecarPolicy` (Task 4); `FerrexServer::with_session`, `SessionDefaults` (Task 5).
- Produces:
  - `pub fn daemon::run(config: FerrexConfig, key: String) -> eyre::Result<()>` (returns `Err` only before binding; otherwise ends with `std::process::exit`)
  - `pub struct IdleState { pub init_ok: bool, pub in_hello: usize, pub active: usize }`, `fn should_arm(&self) -> bool` (`init_ok && in_hello == 0 && active == 0`)
  - `pub fn env_duration(name: &str, default_secs: u64, cap_secs: Option<u64>) -> Duration` plus pure `fn parse_secs(raw: Option<&str>, default: u64, cap: Option<u64>) -> (u64, Option<String>)` (the `String` is the warning; unset → silent default; `0` or unparseable → default with warning naming the var and value; above cap → cap)
  - `struct CollectionMemo(Mutex<HashMap<String, Arc<tokio::sync::OnceCell<()>>>>)` with `async fn ensure(&self, vs: &VectorStore, ns: &str) -> Result<(), CoreError>` (`get_or_try_init`: concurrent hellos share one call, only success is memoised)
  - `enum ConnEvent { HelloPassed, HelloFailed, Ended }` on an `mpsc` channel to the accept loop. Counter mapping: accept → `in_hello += 1` (in the loop itself, no event); `HelloPassed` → `in_hello -= 1; active += 1`; `HelloFailed` → `in_hello -= 1`, terminal (no `Ended` follows); `Ended` (only after `HelloPassed`) → `active -= 1`. The task sends `HelloFailed`/`Ended` from a drop guard that tracks its phase (created in `Hello`, switched to `Active` after the reply), so a panic, abort or early return still reports; the guard's send ignores errors (the receiver may be gone during `abort_all`). Decrements use `checked_sub` with a `debug_assert!`; the counter logic lives in `idle.rs` as `IdleState::apply(ConnEvent)` (test: Step 1's `hello_failed_is_terminal`)
- Test harness (`tests/common/mod.rs`, starting with `#![allow(dead_code)]` like `crates/core/tests/common/mod.rs`, since each test binary uses a subset): `TestEnv { pub temp: TempDir, pub root: PathBuf, pub run_dir: PathBuf, pub db_path: PathBuf, pub config_path: PathBuf, pub qdrant_port: u16, _sidecar: Option<QdrantSidecar> }`, where `root = canonicalize(temp.path())` and every path is built under `root` (on macOS `/var/folders` canonicalises to `/private/var/folders`, and PID files written by tests must match `build_config`'s canonical `db_path`) with `TestEnv::new().await` (`KillOnDrop` sidecar in the temp dir) and `TestEnv::without_sidecar()` (no qdrant; `qdrant_port` is a free unused port; for fake-daemon tests), `fn ferrex_cmd(&self) -> tokio::process::Command` (bin path, `--qdrant-url`, `--db-path`, `--config-path`, `--model-tier small`, env `FERREX_RUN_DIR`, `FERREX_IDLE_TIMEOUT=3`, stderr inherited) and `fn ferrex_cmd_with(&self, opts: CmdOpts) -> tokio::process::Command` where `#[derive(Debug, Clone, Default)] CmdOpts { db_path: Option<PathBuf>, qdrant_url: Option<Option<String>>, extra_args: Vec<OsString>, env: Vec<(String, String)> }` overrides the defaults instead of repeating flags (clap rejects a repeated `--db-path`). Every `CmdOpts` literal ends with `..Default::default()`, so Task 11's added `exe` field breaks no caller. `ferrex_cmd_with` first `env_remove`s every inherited `FERREX_*` var except `FERREX_LOG` (so a developer shell exporting e.g. `FERREX_NO_DAEMON` or `FERREX_NAMESPACE` can't change results), then applies the harness defaults and `CmdOpts.env`; `FASTEMBED_CACHE_DIR` and `HOME` stay inherited so CI's model cache is reused. It emits every top-level flag (defaults plus `CmdOpts` overrides) first and appends `extra_args` after them; `extra_args` starts with the subcommand, because the top-level flags aren't `global = true` and clap rejects them after a subcommand, `async fn paths(&self) -> PathsJson` (runs `daemon-paths`), `struct DaemonGuard { pid: Option<u32>, pid_file: PathBuf }`: `DaemonGuard::for_child(&Child)` records the child's PID at spawn; otherwise the PID is read once from the PID file right after the daemon binds (for daemons spawned by a proxy). `Drop` SIGKILLs that recorded PID, never re-reads the file, and never signals a PID ≤ 1, `async fn raw_session(socket: &Path, key: &str, ns: &str, hello_timeout: Duration) -> (HelloReplyJson, Option<McpConn>)`, and `McpConn::request(&mut self, method, params) -> Value` / `notify`.

Daemon flow, per spec §"Daemon":
1. If `FERREX_TEST_DAEMON_EXIT_BEFORE_BIND` is set, return `Err` right away. Recompute the key from `config` and `current_exe_canonical()`; mismatch → `Err` naming both keys, before binding.
2. Set `config.namespace = "default"`. Build a multi-thread runtime. `ensure_run_dir`. If `socket` connects → `exit(0)`. Sleep `FERREX_TEST_DAEMON_BIND_DELAY_MS` if set. Bind; on `AddrInUse` connect once (`Ok` → exit 0, else log "stale or busy socket, not unlinking" and exit 1). Record socket `(dev, ino)` with `MetadataExt`. Write `PidFile { pid, db_path }`.
3. Init on a `std::thread` with `handle.block_on`, sleeping `FERREX_TEST_INIT_DELAY_MS` first, sending the result over a `oneshot`.
4. One `select!` loop over accept, SIGTERM, SIGINT, init result, init deadline (`FERREX_INIT_TIMEOUT`, default 1800, cap 3000; on expiry the error text contains `timed out`), init-failed grace (`FERREX_TEST_INIT_FAIL_GRACE_MS` or 60 s), idle timer (`FERREX_IDLE_TIMEOUT`, default 600), and the `ConnEvent` channel. Re-evaluate `should_arm` after every event and re-arm with a fresh countdown each time it goes from false to true. Also start a fresh countdown unconditionally after an idle exit abandoned in spec §"Shutdown" step 1 (lock not acquired within 10 s), even though `should_arm` stayed true. Keep init as `enum InitState { Pending(oneshot::Receiver<..>), Ready(Arc<MemoryService>), Failed(String) }`. On `Ok(svc)` from the init result, `let (svc, sidecar) = svc.into_parts();` (as `run_server` does, spec §"Daemon"): keep `sidecar` in an accept-loop local until exit and store `Ready(Arc::new(svc))`, so the sidecar isn't owned by a session's `Arc` clone and `stats` reports no qdrant PID over MCP. The loop also owns a `watch::Sender<InitView>` (`Pending | Ready(Arc<MemoryService>) | Failed(Arc<str>)`) and publishes `Ready`/`Failed` whenever `InitState` leaves `Pending`. only `Pending` enables the init-result and init-deadline arms; on deadline expiry move to `Failed("init timed out after Ns")` and drop the receiver, so a late result is never seen. `Failed` is terminal; only the grace timer and shutdown leave it.
5. Each accepted connection gets a task: read the hello line through a `BufReader` (10 s timeout), wait for init (a `watch::Receiver<InitView>` cloned at accept, `wait_for(|s| !matches!(s, Pending))`), decode/check, `CollectionMemo::ensure` (failure → `Session`), reply, then `FerrexServer::with_session(service.clone(), SessionDefaults { namespace }).serve((reader, writer))` with the same `BufReader`. The tasks live in a `JoinSet` so shutdown can `abort_all`.
6. Shutdown per spec §"Shutdown" steps 1–5, including the idle-exit re-checks, `FERREX_TEST_IDLE_PAUSE_MS` sleep before the drain, ownership-checked removal of `.sock`/`.pid`, `service.shutdown()` under a 10 s timeout, then `sidecar.shutdown()` (a no-op under `LeaveRunning`: qdrant and its PID files stay, and the watcher thread is abandoned at `exit(0)` per spec §"Qdrant sidecar in daemon mode"), `std::process::exit(0)`.

- [x] **Step 1: Write the failing unit tests**

- `idle.rs`: `arms_only_when_init_ok_and_no_connections` covering the four corners.
- `idle.rs`: `hello_failed_is_terminal`: accept, then `HelloFailed` with no `Ended` → `in_hello == 0`, `active == 0`.
- `env.rs`: `parse_secs` for unset → `(600, None)`, `"0"` → `(600, Some(_))`, `"abc"` → `(600, Some(w))` with `w` containing `abc`, `"5000"` with cap 3000 → `3000`.

- [x] **Step 2: Write the failing integration tests** in `daemon_lifecycle.rs` (each starts `ferrex … daemon --key <key from paths()>` with a `DaemonGuard`)

- `two_clients_served_concurrently`: two `raw_session`s both get `{"ok":true}`, both complete `initialize` + `tools/list`.
- `recall_in_fresh_namespace_is_empty`: hello with ns `fresh_<uuid>`; `tools/call recall` with explicit `namespace` of the same → result text parses to `[]`.
- `init_failure_refuses_then_exits`: pre-seed the DB via `SqliteStore::open_with_pool_size(db, 1)` + `set_metadata("embedding_model", "bogus")`, `FERREX_TEST_INIT_FAIL_GRACE_MS=1500`; hello (60 s timeout, to cover a cold model load) gets `kind:"init"` with `embedding model mismatch`; the process exits within 5 s of the hello reply (grace 1.5 s + shutdown).
- `bogus_key_exits_before_binding`: `daemon --key bogus` exits non-zero, no socket file.
- `idle_exit_with_no_connections`: `FERREX_IDLE_TIMEOUT=2`, nobody connects; exits within 60 s (model load + 2 s); socket and PID file removed.
- `connection_during_idle_pause_is_served`: `FERREX_IDLE_TIMEOUT=2`, `FERREX_TEST_IDLE_PAUSE_MS=3000`, no connection at first. Poll `try_lock` on `<key>.lock` every ~50 ms (releasing it at once on success) until it returns `WouldBlock`: with no proxy running, only the daemon's idle shutdown holds it, so this marks the pause (60 s deadline for the cold model load; fail if never seen held). Only then `raw_session`; the session gets `ok` and a `tools/list` answer, and the daemon is still alive after the pause. The daemon's order is: take the lock, then the pause, then the drain.
- `bad_or_silent_hello_does_not_block_idle_exit`: first one good `raw_session` (60 s hello timeout, proves init is done) and disconnect; then one connection sends garbage, one sends nothing; the daemon still exits within idle + 10 s + margin of the good hello's reply.
- `invalid_namespace_gets_session_refusal_others_fine`: hello with ns `"bad/ns"` gets `kind:"session"` while a concurrent default session keeps answering.
- `second_daemon_for_live_key_exits_zero` (idle 600): start a second `daemon --key <same>`; it exits 0; socket inode and PID file content unchanged.
- Tests below marked (idle 600) set `FERREX_IDLE_TIMEOUT=600` so an idle exit can't race the assertion.
- `sigterm_with_lock_held_leaves_files` (idle 600): test takes `File::lock` on `<key>.lock`; SIGTERM; exits within 12 s; socket and PID file still exist.
- `idle_exit_with_lock_held_keeps_serving`: `FERREX_IDLE_TIMEOUT=2`, hold the lock, then one `raw_session` (60 s hello timeout) and disconnect at `t0`; at `t0 + 15 s` the daemon is alive and a new session gets `ok` (use a 20 s hello timeout: the accept loop may be inside its 10 s lock wait).
- `idle_exit_rearms_after_lock_released`: `FERREX_IDLE_TIMEOUT=2`, take the lock, then one `raw_session` (60 s hello timeout, proves init is done) and disconnect at `t0`; hold the lock until `t0 + idle + 10 s + ~2 s` so at least one idle exit is abandoned; release it without connecting again; the daemon exits within idle + 10 s + margin of the release and the socket and PID file are removed. No session may connect after `t0`, so a session end can't re-arm the timer and mask a missing re-arm.
- `init_timeout_reports_timeout`: `FERREX_TEST_INIT_DELAY_MS=5000`, `FERREX_INIT_TIMEOUT=1`; hello gets `kind:"init"` whose error contains `timed out`. With `FERREX_TEST_INIT_FAIL_GRACE_MS=10000`, a second hello sent after the delayed init would have finished (~7 s) still gets `kind:"init"`.
- `shutdown_never_deletes_foreign_files` (idle 600): after the daemon binds, replace `<key>.pid` with `{"pid":1,..}`; SIGTERM; the PID file is still there.
- `shutdown_never_deletes_foreign_socket` (idle 600): after the daemon binds, unlink `<key>.sock` and bind a new `UnixListener` at that path (PID file untouched); SIGTERM; the socket still exists with the new listener's inode. Kept separate from the PID test so an implementation that gates both removals on the PID check can't pass.
- `pid_file_holds_pid_and_db_path` (idle 600): parsed `PidFile` matches the child PID and canonical `db_path`.

- [x] **Step 3: Run to verify failure**

Run: `cargo nextest run -p ferrex-server idle:: env:: && cargo nextest run -p ferrex-server --test daemon_lifecycle`
Expected: compile errors / FAIL.

- [x] **Step 4: Implement** per the flow above.

- [x] **Step 5: Run tests**

Run: `cargo nextest run -p ferrex-server --test daemon_lifecycle && cargo nextest run -p ferrex-server --bins && just clippy`
Expected: PASS.

---

### Task 9: Proxy happy path

**Files:**
- Create: `crates/server/src/proxy.rs`, `crates/server/src/proxy/track.rs`
- Modify: `crates/server/src/cli.rs` (`--no-daemon`), `crates/server/src/main.rs` (routing; remove every `#[allow(dead_code)]` added on `mod` lines)
- Modify: `crates/server/tests/mcp_protocol.rs` (add `--no-daemon`)
- Modify: `crates/server/tests/common/mod.rs` (`FakeDaemon`, `ProxyClient`)
- Test: `crates/server/tests/proxy_fake.rs`, `crates/server/tests/daemon_e2e.rs`

**Interfaces:**
- Consumes: Tasks 3, 6, 7, 8.
- Produces:
  - `Cli.no_daemon: bool`: `#[arg(long, env = "FERREX_NO_DAEMON", action = ArgAction::SetTrue, value_parser = clap::builder::BoolishValueParser::new())]`
  - `pub fn cli::use_standalone(no_daemon: bool, config: &FerrexConfig) -> bool` (`no_daemon || ferrex_store::is_memory_path(&config.db_path)`)
  - `pub fn proxy::run(config: FerrexConfig, argv: Vec<OsString>) -> eyre::Result<()>`; `main` maps the result to an exit code, flushes and calls `std::process::exit`.
  - Lines are bytes end to end, so non-UTF-8 input is forwarded rather than treated as EOF: `pub enum StdinEvent { Line(Vec<u8>), Eof }`, `fn spawn_stdin_thread() -> mpsc::Receiver<StdinEvent>` (blocking `read_until(b'\n')` on a `std::thread`; only `Ok(0)` is EOF, and a read error is logged and treated as EOF), queues are `VecDeque<Vec<u8>>`, and the tracker parses with `serde_json::from_slice`.
  - `enum Out { Line(Vec<u8>), Exit(i32) }`, `type StdoutTx = mpsc::UnboundedSender<Out>`, `fn spawn_stdout_writer() -> StdoutTx` (single writer on a dedicated `std::thread`, like the stdin thread: drains with `blocking_recv` and does blocking writes to a locked stdout, so it never depends on a runtime worker being free; EPIPE → flush stderr, `exit(0)`). `fn exit_via_writer(out: &StdoutTx, code: i32) -> !` sends `Out::Exit(code)`, then sleeps ~1 s and calls `std::process::exit(code)` as a fallback if stdout is blocked. The writer handles `Exit` after every earlier `Line` (FIFO), flushes stdout and stderr, and calls `std::process::exit(code)`. Every proxy exit after the writer starts goes through it (round failure, stdin EOF, Task 13's exhausted path), so synthetic error responses sent just before exit aren't lost.
  - `track.rs`:

```rust
pub enum ClientLine { Request { id: Value, method: String }, Notification { method: String }, Untracked }

pub struct KeptRequest { pub id: Value, pub line: Vec<u8> }

pub struct Tracker { /* in_flight: HashMap<String, (Value, String)>, keyed by serde_json::to_string(&id) */ }

impl Tracker {
    pub fn on_client_line(&mut self, line: &[u8]) -> ClientLine;
    pub fn on_daemon_line(&mut self, line: &[u8]);
    pub fn initialised(&self) -> bool;
    pub fn take_in_flight(&mut self) -> Vec<Value>;
    pub fn is_in_flight(&self, id: &Value) -> bool;
    pub fn cached_initialize(&self) -> Option<&[u8]>;
    pub fn cached_initialized(&self) -> Option<&[u8]>;
    pub fn kept_first(&self) -> Option<&KeptRequest>;
}
```

  Rules from spec §"newline-framed forwarding" and §"Daemon loss and reconnect": a line with `id` + `method` is a request (recorded); `method` only is a notification; anything else, including arrays and non-JSON, is `Untracked`. Every client `notifications/initialized` is cached (replacing any earlier one) regardless of initialised or link state; it is the line `cached_initialized()` returns. While uninitialised, every `initialize` replaces the cached one, and the first request other than `ping`/`server/discover` becomes `kept_first`, replaced only when it gets an error response. A `result` for a recorded method other than `ping`/`server/discover` marks the session initialised and drops `kept_first`; an `error` doesn't.
  - `pub type SharedTracker = Arc<std::sync::Mutex<Tracker>>`. The main loop, the socket-reader task and every wait share it; the lock is never held across an `.await`. This is what lets Task 11's replay read the cached lines while `wait_with_stdin` keeps recording new ones.
  - `async fn wait_with_stdin<F: Future>(fut: F, rx: &mut mpsc::Receiver<StdinEvent>, tracker: &SharedTracker, queue: &mut VecDeque<Vec<u8>>) -> Option<F::Output>`: queues and tracks lines while `fut` runs; `None` on EOF.
  - `Tracker::take_in_flight_except(&mut self, keep: &Value) -> Vec<Value>` (used by Task 13).
  - Items with no non-test caller until Task 11/13 get `#[allow(dead_code, reason = "used by Task N")]`, tagged with the first task that uses them outside tests (each task removes only its own tag): Task 11 → `take_in_flight`, `cached_initialize`, `cached_initialized`, `jsonrpc_error`, `Budgets::replay`, `RoundError::any_child_exited_early`, `DaemonConn::key`; Task 13 → `take_in_flight_except`, `kept_first`, `Connected::attempts_used`, `is_in_flight`. (`AttemptError::child_exited_early` is read by `run_round`, so it needs none.) Run `just clippy` after removing the `mod`-level allows and add whatever else it reports.
  - `pub fn jsonrpc_error(id: &Value, code: i64, message: &str) -> String`

Proxy flow: stdin thread, stdout writer, `exe = current_exe_canonical()` once, `run_round` with `resolve` = `Cli::try_parse_from(std::iter::once(OsString::from("ferrex")).chain(argv.iter().cloned()))` (clap treats the first item as the program name; `argv` itself has no argv[0] and is passed unchanged to the spawn) → `build_config` → `DaemonPaths::for_config` and `attempt` = `connect_or_spawn` + `send_hello(Hello::new(key, config.namespace))`, all inside `wait_with_stdin`. EOF during startup → exit 0 (the guard kills a child not yet accepting). Round failure → stderr + exit 1. On `ok`: flush the queue in order, then forward. Forwarding structure (spec §"newline-framed forwarding" allows this `select!` form): one main loop owns `rx` and `select!`s over stdin events and a "daemon lost" `oneshot` (a fresh one per connection, never a reused `watch`, whose stale value would fire a false loss after reconnect) from a socket-reader task. A failed socket write is handled exactly like the reader's EOF, through one loss path that runs once per connection. The reader task copies daemon lines to the stdout writer and records responses in the tracker; the main loop writes client lines to the socket (directly or via a socket-writer task's channel). Do not move `rx` into a spawned task: Task 11 needs it back in the main loop to call `wait_with_stdin(reconnect(..), &mut rx, ..)` and to apply the disconnected-state rules. Until Task 11, daemon EOF → stderr `ferrex daemon connection lost` and exit 1. `main`: `None` → `if use_standalone(cli.no_daemon, &config) { standalone::run } else { proxy::run(config, std::env::args_os().skip(1).collect()) }`.

Fake daemon (`tests/common/mod.rs`): `FakeDaemon::start(socket: &Path, script: Vec<FakeConn>) -> FakeDaemon`, where `enum FakeConn { Refuse(&'static str), CloseBeforeReply, NeverReply, Serve(FakeBehaviour), DelayHello(u64, FakeBehaviour) }` and `enum FakeBehaviour { Echo, CloseOnRequest(usize), NeverAnswer, DelayMs(u64) }` (Task 13 adds `HoldFirst`). Each accepted connection takes the next script entry, and the last entry repeats. `Echo` answers every request with `{"jsonrpc":"2.0","id":<id>,"result":{"echo":<method>,"params":<params>}}`. `CloseOnRequest(k)` answers requests 1..k-1 like `Echo` (notifications aren't counted) and closes the connection when the k-th request arrives, without answering it. `NeverAnswer` reads and records but never replies. `DelayMs(n)` answers like `Echo` after `n` ms. `FakeDaemon::received(&self) -> Vec<Vec<String>>` returns the lines each connection received after its hello line (the hello is not recorded; a `Refuse`, `CloseBeforeReply` or `NeverReply` connection records an empty `Vec`), per connection in accept order, and `connections(&self) -> usize`. `TestEnv::paths_with(&self, opts: &CmdOpts) -> PathsJson` (runs `daemon-paths` with the same overrides, so a fake listens on the socket the proxy will actually compute). `ProxyClient::spawn(env: &TestEnv, opts: CmdOpts) -> ProxyClient` with stderr piped and drained by a background task, `stderr_text(&self) -> String`, `send(&mut self, line)`, `request(method, params) -> Value`, `read_line_timeout(d) -> Option<String>`, `close_stdin()`, `wait_exit(d) -> Option<ExitStatus>`.

- [x] **Step 1: Write the failing tracker unit tests** in `track.rs`

- `request_recorded_and_cleared`.
- `tracker_distinguishes_numeric_and_string_ids` (Review Focus 3): requests `1` and `"1"`; a response for `"1"` leaves `1` in flight.
- `batch_array_and_garbage_are_untracked` (Review Focus 2): `[{"id":1,"method":"x"}]`, `not json`, `b"\xff\xfe{"` → `Untracked`, nothing in flight.
- `initialize_result_marks_initialised`; `initialize_error_does_not`.
- `ping_result_does_not_initialise`; `tools_call_result_initialises_inline`.
- `kept_first_replaced_only_after_error`: pipeline `a` then `b`; `kept_first` is `a`; error for `a` → next request `c` becomes kept.
- `latest_initialize_cached_until_initialised`.
- `initialized_notification_cached_after_initialise`: `initialize`, its result, then `notifications/initialized` → `cached_initialized()` returns that line and `cached_initialize()` still returns the original.

- [x] **Step 2: Write the failing integration tests**

`proxy_fake.rs` (`TestEnv::without_sidecar()`, fake daemon listening at `paths().socket`):
- `fake_echo_round_trip`: `initialize` and `tools/list` answered through the proxy.
- `non_utf8_line_does_not_stop_proxy` (Review Focus 2): send `b"\xff\n"` then a valid request; the request is answered and `received()[0][0]` is `"\u{fffd}"` (`received()` decodes lines with `String::from_utf8_lossy`).
- `lines_before_ok_are_queued_in_order`: `DelayHello(500, Echo)`; client sends `initialize` and two requests immediately; `received()` is in order.
- `large_line_round_trips` (Review Focus 1): a request whose `params` holds a 5 MB string; within 10 s the client gets `result.params` equal to it, and `received()[0]` holds the full line.
- `three_key_refusals_exit_nonzero`: `Refuse("key")` repeated → proxy exits non-zero; fake saw 3 connections.
- `protocol_refusal_exits_after_one_attempt` and `init_refusal_exits_after_one_attempt`: 1 connection seen.
- `eof_before_hello_reply_is_retried`: `[CloseBeforeReply, Serve(Echo)]` → `initialize` succeeds.
- `silent_hello_ends_round_after_one_attempt`: `NeverReply`, `FERREX_TEST_PROXY_TIMEOUT_MS=500` → exit non-zero, 1 connection.
- `flock_timeout_ends_round`: test holds `<key>.lock`, nothing listening, `FERREX_TEST_PROXY_TIMEOUT_MS=500` → exit non-zero within 3 s (one attempt, not three with backoff), stderr mentions the lock exactly once.
- `unusable_run_dir_fails_fast` (Review Focus 4, proxy path): `FERREX_RUN_DIR` set via `CmdOpts.env` to a regular file, no fake daemon, and no `paths()`/`paths_with()` call (they run `daemon-paths`, which fails here) → exit non-zero within 3 s, stderr names the path exactly once.
- `no_daemon_env_zero_uses_daemon`: `FERREX_NO_DAEMON=0` → the proxy talks to the fake daemon (`connections() == 1`).
- `memory_db_path_selects_standalone`: the fake listens at `paths_with(&opts).socket` for `opts = CmdOpts { db_path: Some(":memory:".into()), ..Default::default() }`; a proxy with the same `opts` and no flag runs standalone, loads models and fails on the unused `--qdrant-url` port: `wait_exit(60 s)` returns `Some(status)` with `!status.success()` and stderr lacks `ferrex ready`; then assert `fake.connections() == 0` and no `.sock`/`.pid` in the run dir.

`daemon_e2e.rs` (real daemon):
- `initialize_tools_list_store_recall_through_proxy`.
- `no_daemon_env_selects_standalone`: `FERREX_NO_DAEMON=1` with real qdrant → `initialize` and `tools/list` work and no `.sock` or `.pid` appears in the run dir.
- `second_proxy_reuses_daemon`: both see the same daemon PID (from the PID file); one PID file in the run dir.
- `concurrent_proxies_spawn_one_daemon`: 5 proxies started together → exactly one live daemon PID file, all five `initialize` successfully.
- `stale_socket_file_is_replaced`: create a regular file at `socket`; the proxy spawns and works.
- `lock_held_by_killed_process_does_not_block` (Review Focus 5): proxy P1 with `FERREX_TEST_DAEMON_BIND_DELAY_MS=3000` holds `<key>.lock` during its accept poll; SIGKILL P1 after 500 ms; proxy P2 then completes `initialize` (it either reuses the orphan daemon once it binds, or spawns its own).
- `daemon_exit_before_bind_fails_fast`: `FERREX_TEST_DAEMON_EXIT_BEFORE_BIND=1` → proxy exits non-zero within 10 s, stderr names `ferrexd.log`, and afterwards `try_lock` on `<key>.lock` succeeds.
- `stdin_close_during_accept_poll_kills_child_and_releases_lock`: `FERREX_TEST_DAEMON_BIND_DELAY_MS=5000`; close stdin after 500 ms; proxy exits 0; `try_lock` on `<key>.lock` succeeds; after waiting 7 s (bind delay plus margin) neither `<key>.sock` nor `<key>.pid` exists, which shows the child was killed before it could bind.
- `accept_timeout_kills_child_and_names_log`: `FERREX_TEST_DAEMON_BIND_DELAY_MS=5000`, `FERREX_TEST_PROXY_TIMEOUT_MS=500` → proxy exits non-zero within 10 s with stderr naming `ferrexd.log`; `try_lock` on `<key>.lock` succeeds; after ~7 s neither `<key>.sock` nor `<key>.pid` exists.
- `fresh_proxy_against_fake_leaves_no_state`: `TestEnv::without_sidecar()`, fake daemon at `paths_with(&opts).socket` for the same `opts` the proxy uses, `--db-path` in a not-yet-existing dir, `FASTEMBED_CACHE_DIR` an empty temp dir, `HOME` a temp dir → after `initialize` + `tools/list`, no SQLite file, no model files, no `qdrant.pid` anywhere in the temp tree.
- `daemon_idles_out_after_last_proxy`: `FERREX_IDLE_TIMEOUT=2`; close the proxy; the daemon PID is gone within 15 s.

- [x] **Step 3: Run to verify failure**

Run: `cargo nextest run -p ferrex-server track:: && cargo nextest run -p ferrex-server --test proxy_fake --test daemon_e2e`
Expected: compile errors / FAIL.

- [x] **Step 4: Implement** the interfaces and flow above; add `--no-daemon` to `mcp_protocol.rs`'s harness command.

- [x] **Step 5: Run the full server suite and clippy**

Run: `cargo nextest run -p ferrex-server && just clippy`
Expected: PASS, including `mcp_protocol`.

- [x] **Step 6: Manual memory check for the PR**

With three proxies on one real daemon, run `lsof -p <pid>` for the daemon and one proxy: only the daemon holds the model files. Record `ps -o rss= -p` for the daemon and one proxy in the PR description.

---

### Task 10: Per-session namespace

**Files:**
- Modify: `crates/server/src/server.rs`
- Modify: `crates/core/src/types.rs` (`TaxonomyRequest`), `crates/core/src/service/taxonomy.rs`, `crates/core/src/service/stats.rs`
- Modify: `crates/server/src/commands.rs` (`diagnose` only)
- Modify: `crates/core/tests/diagnostics.rs`, `crates/core/tests/store_recall.rs`
- Test: `crates/server/tests/daemon_namespace.rs`, `crates/server/tests/mcp_protocol.rs`

**Interfaces:**
- Consumes: `SessionDefaults` (Task 5); harness (Tasks 8, 9).
- Produces:
  - `TaxonomyRequest { pub namespace: Option<String>, pub limit: Option<usize>, pub default_namespace: Option<String> }`. Fallback order: `namespace`, then `default_namespace`, then `config.namespace`. `all_namespaces` is still listed exactly when `namespace` is `None`.
  - `MemoryService::diagnostics(&self, namespace: &str) -> Result<DiagnosticsReport, CoreError>`; `stats()` passes `&req.namespace`; CLI `diagnose` passes `&config.namespace` (clone it before `from_config` consumes the config).
  - `FerrexServer`: `store`, `recall`, `timeline` fill `p.namespace.or_else(|| Some(self.defaults.namespace.clone()))`; `taxonomy` sets `default_namespace: Some(self.defaults.namespace.clone())` and leaves `namespace` as given.
  - Hand-written `get_info` inside the `#[tool_handler(router = self.tool_router)] impl ServerHandler` block (the macro skips generating one when it finds it, `rmcp-macros-3.5.1/src/tool_handler.rs:98`):

```rust
fn get_info(&self) -> ServerConfig {
    ServerConfig::new(
        ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build(),
    )
    .with_server_info(Implementation::new("ferrex", env!("CARGO_PKG_VERSION")))
}
```

- [x] **Step 1: Write the failing core tests**

`store_recall.rs`: `taxonomy_default_namespace_used_when_unscoped`: store into `ctx.namespace`; request `{ namespace: None, default_namespace: Some(ctx.namespace.clone()) }` → `resp.namespace == ctx.namespace`, `total_memories == 1`, `all_namespaces` is `Some` and contains it. Add `default_namespace: None` to the two existing literals.

`diagnostics.rs`: `stats_diagnostics_follow_requested_namespace`: store an entity-bearing memory into namespace A (the ctx) and another into B; `stats` for B with `diagnostics: Some(true)` → `diag.collection == B`, `diag.entity_count` equals B's entity count, `diag.memory_count` counts both. Update `svc.diagnostics()` → `svc.diagnostics(&ctx.namespace)`.

- [x] **Step 2: Write the failing server tests**

`daemon_namespace.rs`:
- `two_namespaces_on_one_daemon_stay_separate`: proxies with `FERREX_NAMESPACE=ns_a` and `ns_b` share one daemon (one PID file); each stores, recalls and runs `timeline` on its own content only.
- `explicit_namespace_param_wins`: the `ns_a` session stores with `namespace: "ns_b"`; the `ns_b` session recalls it.
- `taxonomy_unscoped_uses_session_namespace`: the `ns_b` session's `taxonomy` reports `namespace: "ns_b"` and lists both namespaces.

`mcp_protocol.rs`: `initialize_advertises_list_changed_and_ferrex_server_info`: `result.capabilities.tools.listChanged == true`, `result.serverInfo.name == "ferrex"`, `result.serverInfo.version == env!("CARGO_PKG_VERSION")`.

- [x] **Step 3: Run to verify failure**

Run: `cargo nextest run -p ferrex-core --test store_recall --test diagnostics; cargo nextest run -p ferrex-server --test daemon_namespace --test mcp_protocol`
Expected: compile errors / FAIL.

- [x] **Step 4: Implement** the interfaces above. Tool descriptions and output shapes don't change.

- [x] **Step 5: Run tests**

Run: `cargo nextest run -p ferrex-core && cargo nextest run -p ferrex-server && just clippy`
Expected: PASS.

---

### Task 11: Reconnect for initialised sessions

**Files:**
- Create: `crates/server/src/proxy/reconnect.rs`
- Modify: `crates/server/src/proxy.rs` (replace the Task 9 exit-on-loss with a call into reconnect; add the connection state), and every other file carrying one: remove each `#[allow(dead_code, reason = "used by Task 11")]` added in Task 9 (`grep -rn 'used by Task 11' crates/server/src`), then run `just clippy`
- Modify: `crates/server/tests/common/mod.rs` (add `CmdOpts::exe: Option<PathBuf>`, used by `ferrex_cmd_with` and so by `paths_with` and `ProxyClient::spawn`, in place of the cargo binary; extra fake behaviour only if a test below needs one)
- Test: `crates/server/tests/daemon_reconnect.rs`

**Interfaces:**
- Consumes: `Tracker`, `StdoutTx`, `wait_with_stdin`, `jsonrpc_error` (Task 9); `run_round`, `send_hello`, `Budgets`, `DaemonConn` (Task 7).
- Produces:
  - `pub const RESTART_CODE: i64 = -32603;`, `pub const RESTART_MESSAGE: &str = "ferrex daemon restarted, retry";`
  - `pub enum ConnState { Connected { writer: OwnedWriteHalf, lost: oneshot::Receiver<()>, reader: JoinHandle<()>, key: String }, Disconnected, Reconnecting }`. `reconnect` still returns a whole `DaemonConn` (replay reads through its reader); after success the main loop spawns a new reader task on `conn.reader` with a fresh `oneshot` and builds `Connected`. The loss path (reader EOF/error or a failed socket write, once per connection): move to `Disconnected`, abort the old reader task, call `on_daemon_loss`.
  - `pub enum LossAction { WaitForRequest, ReconnectNow }`
  - `pub fn on_daemon_loss(tracker: &SharedTracker, out: &StdoutTx) -> LossAction`: answers every in-flight id with `RESTART_CODE`/`RESTART_MESSAGE` and clears it (Task 13 adds the pre-init branches).
  - `pub enum ReplayMode { Drop, Forward }` (Task 11 declares only `Drop`; Task 13 adds `Forward`)
  - `pub async fn replay(conn: &mut DaemonConn, tracker: &SharedTracker, mode: ReplayMode, timeout: Duration) -> Result<Option<Vec<u8>>, AttemptError>`: reads the cached lines under the lock right before writing (so a `notifications/initialized` cached while connecting is included), then releases it. `Drop` writes the cached `initialize` without tracking its id, reads until the response to that id and discards it (EOF or error response → retryable; timeout → not retryable), then writes the cached `notifications/initialized` if there is one. `Drop` requires a cached `initialize`; for an initialised inline-lifecycle session the caller picks `AfterHello::Nothing` instead (rmcp closes an inline connection whose first line isn't a request, so a cached `notifications/initialized` must never be sent there). `Forward` (Task 13) returns the response line instead of discarding it; the caller feeds it to the tracker and stdout.
  - `pub enum AfterHello { ReplayDrop, ReplayForward, ResendKept, Nothing }` (Task 11 declares only `ReplayDrop` and `Nothing`; Task 13 adds `ReplayForward` and `ResendKept`)
  - `pub async fn reconnect(ctx: &mut ReconnectCtx, after: AfterHello, used: u8) -> Result<Connected<(DaemonConn, Option<Vec<u8>>)>, RoundError>`: `run_round` whose attempt does `connect_or_spawn` + `send_hello`, then: `ReplayDrop` → `replay(Drop)`; `ReplayForward` → `replay(Forward)` and return its response line; `ResendKept` → write `kept_first().line` (Task 13); `Nothing` → no replay, the caller flushes the queue. Both replay modes get `budgets.replay` as their timeout. Task 11 uses only `ReplayDrop` (and `Nothing` for an initialised inline-lifecycle session, which has no cached `initialize`); the other variants are added in Task 13. `ReconnectCtx` holds `exe`, `argv`, `budgets`, `startup_mtime`, `last_key` and `tracker: SharedTracker`. The caller clones the tracker handle first (`let tracker = Arc::clone(&ctx.tracker);`) and wraps the `reconnect` future in `wait_with_stdin(reconnect(&mut ctx, after, used), &mut rx, &tracker, &mut queue)`, using the proxy's stdin receiver and pending-line queue, so lines arriving during the round are tracked and queued.
  - After a successful reconnect: if the session was already initialised when the daemon was lost (capture `tracker.initialised()` on the loss path, since re-sending a kept request can initialise it mid-round) and `conn.key` differs from `ctx.last_key`, write `{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}` to stdout. Then set `ctx.last_key = conn.key.clone()` on every successful reconnect, whether or not the key changed or the session was initialised (`last_key` starts as the startup connection's key). Then flush the queue in order and resume forwarding.
  - Failed round after init: under the lock, write one `jsonrpc_error(id, -32603, msg)` for each id from `take_in_flight()` (queued requests are already tracked as in flight, so don't walk the queue separately); then `queue.clear()` so already-answered lines never reach the next connection; then `link = Down`, `replay_started = false`, `ConnState::Disconnected`. `msg` carries the cause, and appends `run /mcp reconnect so Claude starts the new ferrex binary` when the last error is `Protocol`, when `exe` no longer exists, or when `any_child_exited_early` and the exe mtime differs from `startup_mtime`. The proxy stays up.

Disconnected-state rules per spec §"Daemon loss and reconnect", "Initialised" steps 1–4: notifications arriving while disconnected are dropped, except `notifications/initialized`, which is cached; the next client request triggers the reconnect; lines arriving while reconnecting are queued, except a `notifications/initialized` that arrives before replay has read the cache (cached only, never queued); one arriving after that is queued and forwarded normally; stdin EOF while disconnected or reconnecting → exit 0. To support this, Task 11 adds `Tracker::link: Link { Up, Down, Reconnecting }` (set by the main loop under the lock on every `ConnState` change; `ConnState` stays in `proxy.rs`), `Tracker::replay_started: bool` (set by `replay` under the lock when it reads the cache, cleared when the round ends) and `ClientLine::CacheOnly`. `on_client_line` decides from `link`: `Up` → normal forwarding; `Down` + notification → returns `Notification` and the main loop discards it, except `notifications/initialized`, which is cached; `Down` + request → recorded and pushed to `queue` (flushed after replay with the lines queued during the round), then the main loop starts the reconnect; `Down` + `Untracked` → dropped like a notification, no reconnect (spec step 2 reconnects only on a request, and a queued untracked line could reach the new connection ahead of the replayed `initialize`); `Reconnecting` + `!replay_started` + `notifications/initialized` → `CacheOnly`; any other line while `Reconnecting` → queued. `wait_with_stdin` doesn't queue `CacheOnly` lines. Test: Step 1's `initialized_during_reconnect_sent_once`.

- [x] **Step 1: Write the failing integration tests** in `daemon_reconnect.rs` (fake daemon unless noted; the client is initialised first)

- `initialized_during_reconnect_sent_once`: client sends `notifications/initialized` again while the reconnect connection is `DelayHello(500, Echo)`; `received()[1]` contains it exactly once.
- `in_flight_request_gets_restart_error`: `[Serve(CloseOnRequest(2)), Serve(Echo)]` (answers `initialize`, closes on the `tools/call`): that call gets an error with code `-32603` and message `ferrex daemon restarted, retry`.
- `next_request_reconnects_and_replays`: `[Serve(CloseOnRequest(2)), Serve(Echo)]`; after the loss, the next `tools/list` succeeds; `received()[1]` is `initialize`, then `notifications/initialized`, then `tools/list`, and the client never saw a second `initialize` response.
- `lines_during_reconnect_arrive_in_order`: second connection is `DelayHello(500, Echo)`; three requests sent back to back arrive in order.
- `key_change_sends_list_changed`: the test rewrites `ferrex.toml` (a dedup threshold) between the loss and the next request and starts a second fake on the new key's socket; the client receives `notifications/tools/list_changed`. `same_key_reconnect_sends_no_list_changed`. `key_change_then_same_key_reconnect_sends_list_changed_once`: lose the daemon, change the key, reconnect (one `list_changed`), lose it again, reconnect on the same key; the client got exactly one `list_changed`.
- `protocol_refusal_after_init_returns_mcp_hint`: second connection refuses `protocol`; the pending request's error message contains `/mcp reconnect`; the proxy is still running.
- `failed_round_then_next_request_succeeds`: `[Serve(CloseOnRequest(2)), CloseBeforeReply, CloseBeforeReply, CloseBeforeReply, Serve(Echo)]`; the request that closes the first connection is the loss and gets the restart error; the next request's round uses up the three `CloseBeforeReply` and errors; the request after that succeeds. Variant: three requests sent during the failing round each get exactly one `-32603` (`ProxyClient::request` matches response ids), and the successful connection's `received()` contains none of them.
- `stalled_replay_ends_round`: the reconnect connection is `Serve(NeverAnswer)`; `FERREX_TEST_PROXY_TIMEOUT_MS=500` → the request errors after one attempt (`connections()` grew by one). `eof_during_replay_is_retried`: `[Serve(CloseOnRequest(2)), Serve(CloseOnRequest(1)), Serve(Echo)]` → the request after the loss succeeds.
- `notifications_while_disconnected_are_dropped`: a `notifications/cancelled` sent while disconnected never shows up in `received()`; `notifications/initialized` sent then does.
- `stdin_eof_while_disconnected_exits_zero`.
- `inline_lifecycle_reconnects_without_replay`: the client never sends `initialize`, its first `tools/call` carries `_meta`; after success and a loss, the next request succeeds, and the second connection's first line is that request (no `initialize`). Covers Task 11's `AfterHello::Nothing` path.
- `stdin_eof_during_reconnect_round_exits_zero`: `[Serve(CloseOnRequest(2)), DelayHello(2000, Echo)]`; after the loss send a request to start the round, close stdin after ~200 ms → exit 0 well before 2 s. (The guard killing a half-spawned child on this path shares code with Task 9's `stdin_close_during_accept_poll_kills_child_and_releases_lock`.)
- `missing_exe_after_init_returns_mcp_hint`: copy the ferrex binary into `env.root` (never delete the shared cargo binary) and set `opts.exe = Some(copy)`; start the fake at `paths_with(&opts).socket` with `[Serve(CloseOnRequest(2))]`, computed before deleting the copy; spawn the proxy with `opts` and initialise; delete the copy. Request A closes the connection and gets `-32603 "ferrex daemon restarted, retry"`. Request B starts the round, where `resolve` fails 3 times (exe missing): B's error contains `/mcp reconnect`, the proxy is still running, and `fake.connections() == 1`.
- Unit test in `reconnect.rs`: pull the hint check into a pure `fn needs_mcp_hint(last: &AttemptError, exe_exists: bool, any_child_exited_early: bool, mtime_now: Option<SystemTime>, startup_mtime: Option<SystemTime>) -> bool` and cover all three triggers (protocol, missing exe, early exit + changed mtime) plus the no-trigger case.
- `real_daemon_kill9_next_request_succeeds`: real daemon via the proxy; `initialize`, `store`; SIGKILL the daemon PID; the next `recall` succeeds without the client sending `initialize` again.

- [x] **Step 2: Run to verify failure**

Run: `cargo nextest run -p ferrex-server --test daemon_reconnect`
Expected: FAIL (the proxy exits on loss).

- [x] **Step 3: Implement** the interfaces above.

- [x] **Step 4: Run tests**

Run: `cargo nextest run -p ferrex-server && just clippy`
Expected: PASS; all Task 9 suites stay green.

---

### Task 12: Admin guard

**Files:**
- Modify: `crates/server/src/daemon/addr.rs`
- Create: `crates/server/src/admin_guard.rs`; `mod admin_guard;` in `main.rs`
- Modify: `crates/server/src/audit.rs` (guard when `fix`), `crates/server/src/backfill.rs` (guard unless `dry_run`)
- Test: `crates/server/tests/daemon_admin_guard.rs`

**Interfaces:**
- Consumes: `PidFile`, `run_dir` (Task 6); `ferrex_store::pid_alive`.
- Produces:
  - `pub fn live_for_db(run_dir: &Path, db_path: &Path) -> Option<u32>`: scans `*.pid`; a PID file that can't be read or parsed is skipped; live means `db_path` equal, `pid_alive(pid)`, and `std::os::unix::net::UnixStream::connect(<stem>.sock)` succeeds.
  - `pub fn any_live(run_dir: &Path) -> Option<u32>`: same checks without the `db_path` filter.
  - `pub fn admin_guard::refuse_if_live(config: &FerrexConfig) -> eyre::Result<()>`: skips an in-memory `db_path`; refusal text: `a ferrex daemon (pid {pid}) is using {db_path}. Close your Claude sessions or wait for the daemon to exit when idle (FERREX_IDLE_TIMEOUT), then retry.`
  - `pub fn admin_guard::refuse_if_any_live() -> eyre::Result<()>`: same wording, naming the pid and "the qdrant sidecar in ~/.ferrex". It and `any_live` carry `#[allow(dead_code, reason = "used by Task 14")]`; Task 14 removes them.

- [x] **Step 1: Write the failing unit tests** in `addr.rs`, using a temp run dir, a PID file with `std::process::id()`, and a `UnixListener` bound at the matching `.sock`

- `live_for_db_finds_same_db_any_key`.
- `live_for_db_ignores_other_db`.
- `live_for_db_ignores_stale_pid_file`: PID is alive (our own) but no socket listening → `None`.
- `live_for_db_skips_unparseable_pid_file`: garbage `x.pid` alongside a valid live one → still finds the valid one, no panic.
- `any_live_finds_daemon_on_other_db`.

- [x] **Step 2: Write the failing integration tests** in `daemon_admin_guard.rs`. A "live daemon" is a test-held `UnixListener` plus a PID file naming the test process, so no model load is needed for refusals.

- `reconcile_fix_refuses_with_live_daemon_on_same_db`: `audit reconcile --fix` exits non-zero, stderr contains `Close your Claude sessions`.
- `backfill_refuses_with_live_daemon_on_same_db`.
- `read_only_commands_run_with_live_daemon`: `audit reconcile` (no `--fix`) and `backfill normalized-predicates --dry-run` succeed.
- `all_succeed_with_no_daemon`.
- `stale_pid_file_does_not_block`: PID file present, no listener → `--fix` succeeds.

- [x] **Step 3: Run to verify failure**

Run: `cargo nextest run -p ferrex-server addr:: && cargo nextest run -p ferrex-server --test daemon_admin_guard`
Expected: FAIL.

- [x] **Step 4: Implement** the interfaces and the two call sites, guarding before `MemoryService::from_config`.

- [x] **Step 5: Run tests**

Run: `cargo nextest run -p ferrex-server && just clippy`
Expected: PASS.

---

### Task 13: Reconnect before init and inline lifecycle

**Files:**
- Modify: `crates/server/src/proxy/reconnect.rs`, `crates/server/src/proxy.rs`, `crates/server/src/proxy/track.rs` (remove each remaining `#[allow(dead_code, reason = "used by Task 13")]`, found with `grep -rn 'used by Task 13' crates/server/src`, then run `just clippy`)
- Modify: `crates/server/tests/common/mod.rs`: add `FakeBehaviour::HoldFirst { close_on: Option<usize> }`, which never answers the connection's first request, echoes later ones, and closes without answering when the k-th request arrives if `close_on = Some(k)`.
- Test: `crates/server/tests/daemon_reconnect.rs`

**Interfaces:**
- Consumes: everything from Task 11; `Tracker::kept_first`, `cached_initialize` (Task 9); `run_round(.., used)` (Task 7).
- Produces:
  - `on_daemon_loss` gains the pre-init branches, using `take_in_flight_except`: `initialize` in flight → answer the other in-flight ids, `ReconnectNow`; inline first request in flight → answer the others, `ReconnectNow`; otherwise answer all, `WaitForRequest`.
  - `AfterHello::ReplayForward` (initialize in flight): re-send the cached `initialize` as a tracked request; `reconnect` returns its response line, which the caller feeds to the tracker and writes to stdout (EOF before the response → retryable; timeout under `budgets.replay` → not retryable). `AfterHello::ResendKept` (inline first request in flight). `AfterHello::Nothing` for the `WaitForRequest` path before init: the client's new request is just flushed from the queue, so the daemon sees exactly one `initialize`.
  - `pub struct ResendPending { pub id: Value, pub attempts_used: u8 }` held by the proxy while the inline kept line's response is outstanding. On a loss while pending: answer the other in-flight ids; if `attempts_used >= MAX_ATTEMPTS` the round has failed, so write `ferrex daemon connection lost` to stderr and exit 1 without calling `reconnect`; otherwise call `reconnect(.., AfterHello::ResendKept, attempts_used)`, which re-sends only the kept line. The round ends when that id's response arrives (checked with `tracker.is_in_flight(&pending.id)` returning false after each forwarded daemon line) or the session becomes initialised.
  - Before init, a failed round → stderr + exit non-zero.
  - Pre-init disconnected state: notifications are dropped except `notifications/initialized` (cached). No `list_changed` is sent for a pre-init loss (the client hasn't listed tools), though `last_key` is still updated; for `ReplayForward`, the forwarded `initialize` response is the first thing written to stdout. Test: Step 1's `pre_init_key_change_sends_no_list_changed`.

- [x] **Step 1: Write the failing integration tests** in `daemon_reconnect.rs`

- `pre_init_key_change_sends_no_list_changed`: daemon lost while `initialize` is in flight, key changed before the reconnect; the client's first message is the `initialize` result and no `list_changed` arrives.
- `ping_in_flight_before_init_gets_restart_error`: `[Serve(CloseOnRequest(1)), Serve(Echo)]` with the client sending only a `ping` first; the ping gets `-32603`; the next `initialize` reconnects and succeeds, and `received()[1]` contains exactly one `initialize`.
- `stalled_forwarded_initialize_ends_round_after_one_attempt`: the first connection closes while the client's `initialize` is in flight, the reconnect connection is `Serve(NeverAnswer)`, `FERREX_TEST_PROXY_TIMEOUT_MS=500` → the proxy exits non-zero after one extra connection.
- `close_before_initialize_response_is_retried`: `[Serve(CloseOnRequest(1)), Serve(Echo)]`; the client sends `initialize` once and gets one success response.
- `pre_init_failed_round_exits_nonzero`: `Serve(CloseOnRequest(1))` repeated → proxy exits non-zero after four connections (the original plus three re-sends).
- `stdin_eof_while_reconnect_pending_exits_zero`: `[Serve(CloseOnRequest(1)), DelayHello(2000, Echo)]`; the client's first line is `initialize` (id 1), the first connection closes on it, so `on_daemon_loss` returns `ReconnectNow` and the `ReplayForward` round starts; once `fake.connections() == 2`, wait ~200 ms and close stdin → exit 0 well before 2 s, nothing written to stdout. (Pre-init counterpart of Task 11's `stdin_eof_during_reconnect_round_exits_zero`.)
- `inline_first_request_resent_after_close`: `[Serve(CloseOnRequest(1)), Serve(Echo)]`; the client gets its response without re-sending.
- `inline_pending_resend_lets_later_lines_through`: `[Serve(CloseOnRequest(1)), Serve(HoldFirst { close_on: None })]`; no `initialize`, the first line is `tools/call` id 1 with `_meta`; the first connection closes on it and the proxy re-sends id 1 to the second, which holds it. The client then sends `tools/list` id 2 (gets its echo response) and `notifications/cancelled` with `requestId: 1`; poll until `received()[1]` is, in order, the id-1 `tools/call`, the id-2 `tools/list`, then the cancelled notification; no `initialize` in it and no response for id 1.
- `inline_repeated_loss_resends_alone_then_exits`: first connection `CloseOnRequest(1)`, then `HoldFirst { close_on: Some(2) }` repeated (the client sends one more request on each connection, so it is in flight at the close); losses while pending re-send only the kept line and give other in-flight ids `-32603`; the fourth loss counting the original → proxy exits non-zero.
- `inline_later_success_initialises`: `[Serve(CloseOnRequest(1)), Serve(HoldFirst { close_on: Some(3) })]`; while the kept id is pending, a later request succeeds; then a loss gives the kept id `-32603` and the proxy stays up.

- [x] **Step 2: Run to verify failure**

Run: `cargo nextest run -p ferrex-server --test daemon_reconnect`
Expected: the new tests FAIL.

- [x] **Step 3: Implement** per spec §"Lifecycle modes" and the two "Not yet initialised" bullets.

- [x] **Step 4: Run tests**

Run: `cargo nextest run -p ferrex-server && just clippy`
Expected: PASS.

---

### Task 14: nuke and docs

**Files:**
- Modify: `crates/server/src/commands.rs` (`nuke`)
- Modify: `README.md`, `docs/design/ferrex-rag-memory.md` (line 112, "stops on shutdown"), `docs/design/design-decisions.md` (line 88, "On ferrex exit → SIGTERM")
- Test: `crates/server/tests/daemon_admin_guard.rs`

**Interfaces:**
- Consumes: `refuse_if_live`, `refuse_if_any_live` (Task 12); `pid_alive`, `is_qdrant`, `probe_qdrant`, `read_pid`, `read_port_file` (Task 2).
- Produces:
  - `fn is_sidecar_dir(dir: &Path) -> bool`: `canonicalize(dir) == canonicalize(home/.ferrex)`, false if either fails.
  - `nuke`: if `is_sidecar_dir(db parent)` → `refuse_if_any_live()?`, then stop qdrant, in this order: (1) no `qdrant.pid`, unparseable, or `!pid_alive(pid)` → nothing to stop, no "skipped" message; (2) else if `is_qdrant(pid, &config.qdrant_bin)` → kill; (3) else if `runtime()?.block_on(probe_qdrant(port))` succeeds (port: `read_port_file(dir/qdrant.port).filter(|(p, _)| *p == pid).map(|(_, port)| port).unwrap_or(config.qdrant_port)`) → kill; (4) otherwise print that the kill was skipped and why. Kill: `rustix::process::kill_process(pid, Signal::TERM)`, poll `pid_alive` every 100 ms up to 10 s, then `Signal::KILL`; `ESRCH` from either signal counts as success, not an error. If not the sidecar dir → `refuse_if_live(config)?` and leave qdrant alone. Then delete files as today plus `qdrant.port`. Order inside `nuke`: the guard (`refuse_if_any_live` or `refuse_if_live`) runs first, before the "This will delete" listing and any early return; stopping qdrant runs after the confirmation prompt, so answering "N" never kills the sidecar. `has_anything` also counts `qdrant.pid` and `qdrant.port`, so a leftover PID file alone doesn't hit today's "(nothing found)" early return.

- [x] **Step 1: Write the failing integration tests** in `daemon_admin_guard.rs`. Every `nuke` subprocess runs with `HOME=<temp>` so `~/.ferrex` is `<temp>/.ferrex`, `--force`, `FERREX_QDRANT_PORT=<free port>` via `CmdOpts.env` (`CmdOpts` has no port field, and the env binding works regardless of flag position; so the probe can never hit a developer's sidecar on 6334), and `ferrex_cmd_with` for the db path. Sidecars started by these tests use `KillOnDrop`: a `LeaveRunning` watcher in the test process would respawn qdrant after `nuke` kills it. A killed `KillOnDrop` child is a zombie, which `pid_alive` already reports as dead.

- `nuke_default_path_refuses_with_daemon_on_other_db`: fake live daemon whose PID file names `/elsewhere/x.db`; `nuke` on `<temp>/.ferrex/ferrex.db` exits non-zero.
- `nuke_custom_path_refuses_only_for_same_db_and_leaves_qdrant`: a `KillOnDrop` sidecar in `<temp>/.ferrex` on the free port; `nuke --db-path <temp>/custom/x.db` with a fake daemon on another DB succeeds and the qdrant PID is still alive; with a fake daemon on `<temp>/custom/x.db` it refuses.
- `nuke_default_path_with_qdrant_url_still_stops_sidecar`: `KillOnDrop` sidecar in `<temp>/.ferrex` on the free port, no daemons, `nuke --qdrant-url http://localhost:1` → the qdrant PID is gone within 12 s; `qdrant.pid` and `qdrant.port` deleted.
- `nuke_skips_kill_when_pid_is_not_qdrant`: only `qdrant.pid` exists, naming a `sleep 60`; nothing listens on the free port → output says the kill was skipped, `sleep` still alive, `qdrant.pid` deleted.
- `nuke_with_dead_qdrant_pid_deletes_files`: `qdrant.pid` names a reaped PID → `nuke` succeeds, output has no "skipped" message, files deleted.

- [x] **Step 2: Run to verify failure**

Run: `cargo nextest run -p ferrex-server --test daemon_admin_guard`
Expected: the new tests FAIL.

- [x] **Step 3: Implement** `nuke` as above.

- [x] **Step 4: Update docs**

- `README.md`: a short "Shared daemon" section under Architecture: the first session spawns one daemon per database/config, later sessions reuse it, it exits after `FERREX_IDLE_TIMEOUT` (default 600 s), logs go to `~/.ferrex/run/ferrexd.log`, `--no-daemon` / `FERREX_NO_DAEMON=1` runs the old in-process server, the qdrant sidecar stays running after the last session and `ferrex nuke` stops it, and `audit reconcile --fix`, `backfill` and `nuke` refuse while a daemon uses the database.
- `docs/design/ferrex-rag-memory.md:112`: daemons leave the sidecar running; only `--no-daemon` sessions and CLI commands stop a qdrant they spawned; `nuke` stops it.
- `docs/design/design-decisions.md:88`: a dated note (2026-10-18) that daemons skip this step, pointing to `docs/specs/2026-10-14-shared-daemon-design.md`.

- [x] **Step 5: Run tests and lint**

Run: `cargo nextest run -p ferrex-server && just clippy && just lint`
Expected: PASS (`typos` and `taplo` clean).

---

### Final check (after Polish)

Polish edits code in its review, idiomatic and cleanup passes and runs no tests, so re-verify the whole branch after it:

- [ ] `just test && cargo nextest run -p ferrex-store --run-ignored all && just clippy && just check-fmt && just lint` → PASS.
- [ ] `rg 'used by Task' crates/` → no matches (every interim `dead_code` allow removed).
- [ ] Spec T1–T4 "done" bullets each map to a passing test named in this plan; list any bullet without one.
