# Shared ferrex daemon

## Why

Every Claude Code session starts its own `ferrex` over stdio, and each process loads the
embedding model and the reranker on its own. Measured on aarch64-darwin with the default tiers:

| state | phys footprint |
|---|---|
| embedder (`bge-base-en-v1.5`) loaded | ~600 MB |
| + reranker (`bge-reranker-base`) loaded | ~1.86 GB (3.1 GB peak during load) |
| after a few recalls (rerank pool 20–40 docs, one batch) | 2.5–4.4 GB |

ONNX Runtime keeps its arena at the high-water mark, so a session that has done a handful of
recalls sits at 2+ GB, and five open sessions cost about 10 GB. Qdrant already runs once and
is shared through a PID file. The models should be shared the same way.

## Goals

- One long-lived process per (binary, resolved config) holds the models, SQLite, caches and the
  Qdrant client, no matter how many Claude sessions are open. In the usual setup that means one
  per database.
- Nothing changes for the user. The Claude MCP config still runs `ferrex` over stdio, the first
  session spawns the daemon on its own, and later sessions reuse it.
- When the last session closes, the daemon exits after an idle timeout and frees the RAM.
- Tools, tool parameters and their results don't change, with one deliberate exception:
  `stats` with `diagnostics: true` reports the requested namespace's entity count and
  collection instead of `config.namespace`'s (see "Per-session default namespace").

## Non-goals

- HTTP / Streamable HTTP transport. Claude does not spawn URL-based servers, and auto-spawn is
  a requirement.
- Running the daemon under launchd or systemd.
- Windows. The flake targets Linux and macOS only, so Unix domain sockets are fine.
- Reducing the model footprint itself (batched reranking, lazy reranker load, smaller tiers).
  Those are listed as follow-ups.

## Anti-goals

- A session silently running with different settings than it asked for (wrong DB, wrong model
  tier, wrong default namespace). If this happens, the design has failed even if memory drops.
- Any output on the proxy's stdout other than MCP frames from the daemon.
- A crash or upgrade leaving sessions stuck with no way back short of restarting Claude.

## Architecture

```
claude#1 ─stdio─> ferrex (proxy) ─┐
claude#2 ─stdio─> ferrex (proxy) ─┼─ unix socket ─> ferrex daemon ─> MemoryService
claude#3 ─stdio─> ferrex (proxy) ─┘                     │              ├─ Embedder / Reranker (1x)
                                                        │              ├─ SQLite
                                                        │              └─ Qdrant client ─> qdrant sidecar
                                                        └─ one FerrexServer (rmcp) per connection
```

### Decision: whole ferrex in the daemon behind a thin stdio proxy (decided by the user)

- Pros: per-session cost drops to a few MB. One process per config owns SQLite and the caches. The proxy
  only inspects JSON-RPC ids, method names and error-vs-success, enough for the replay and
  lifecycle rules in "Daemon loss and reconnect (proxy side)"; it doesn't implement MCP.
- Cons: per-session settings have to be sent to the daemon in a handshake. A daemon crash
  affects every session at once (handled by reconnect, below). The daemon serves several MCP
  sessions concurrently.
- Rejected alternative: a models-only daemon with each session keeping core and SQLite. It
  needs a separate embedding RPC protocol, still costs tens of MB per session, and keeps
  several processes writing to one SQLite file.

### Decision: one daemon per (binary, resolved config), keyed in the socket name (my call)

The proxy computes a `key`: the first 16 hex chars of SHA-256 over
1. the canonical path of the current executable,
2. its mtime, and
3. the fully resolved `FerrexConfig` with `namespace` removed. For hashing only, `qdrant_bin`
   is replaced by its absolute path resolved through `PATH`, so two sessions whose `PATH`s
   resolve to different qdrant binaries get different daemons. Those daemons still share the one
   sidecar in `~/.ferrex`, whichever binary started it; a port mismatch with the running sidecar
   is caught at reuse (see "Qdrant sidecar in daemon mode"). If `qdrant_url` is set, or the
   binary can't be found, the raw string is hashed instead. That's never an error, because
   Docker-qdrant users have no qdrant binary at all. The config itself keeps the raw value. The
   proxy and the daemon go through the same key function, so they agree. That covers CLI flags, env vars
   and everything loaded from `ferrex.toml`: `db_path`, tiers, qdrant url/bin/port, config path,
   dedup, conflict, predicates, staleness, cache, reader pool size, and so on.

To serialise the config, derive `Serialize` on `FerrexConfig` and every type it contains. None
have it today, except `ferrex_store::MemoryType` (the `HashMap` key of
`StalenessConfig.type_config`), which already derives `Serialize` as a lowercase string;
`to_value` turns that `HashMap` into a sorted map, so its iteration order doesn't affect the
hash. The others live in `crates/core/src/types.rs`, `crates/core/src/staleness.rs`
(`StalenessConfig`, `StalenessWeights`, `TypeStalenessConfig`) and `crates/embed/src/lib.rs`
(`ModelTier`, `RerankerTier`; `ferrex-embed` gains a `serde` dependency from the workspace).
`config.rs` (the `*File` types, `LoadedConfig` and the loaders) doesn't change. Hash the output of
`serde_json::to_value`, whose maps are sorted as long as `serde_json/preserve_order` is off,
which it is today. The `Vec`s inside the config (predicate group members) come straight from TOML
arrays in file order, not from iterating a map, so they're stable too.

Key tests:
- a destructure of `FerrexConfig` with no `..`, so adding a field breaks the build until the
  key code is looked at;
- a per-field perturbation test showing that each field changes the key;
- the same config with several predicate groups hashed in two separate processes gives the
  same key. This catches `preserve_order` getting switched on through feature unification.

On Nix every store path has mtime 1, so there the key effectively depends on the store path.
That is fine and intended.

The proxy passes its key to the daemon (`daemon --key <k>`). The daemon builds its own config
from the same argv and env, recomputes the key and compares. If they differ, because
`ferrex.toml` was edited or the binary rebuilt between the two, the daemon returns an error from `main`
naming both keys (color_eyre prints it to stderr, so it lands in `ferrexd.log` even when
`FERREX_LOG_FILE` is set) and exits before binding. The proxy sees the child exit (below), counts a failure,
and on the retry recomputes its key from the new state. So a daemon never runs one config under
another config's key.

`cli::build_config` makes `db_path` and `config_path` absolute in every mode. For each one it
runs `create_dir_all(parent)` (an empty parent from a bare relative path means the cwd), then
`canonicalize(parent)`, then joins the file name. `config_path` is handled after `load_or_init`
has created the file. This works on a fresh machine, where neither `~/.ferrex` nor the db
directory exists yet. The key doesn't depend on the cwd, and different spellings of the same
path give the same key. `MemoryService` already creates the db directory, so standalone mode
and the admin commands see no behaviour change, apart from `nuke` creating the db directory on a fresh machine (it never opens
`MemoryService`), and `diagnose`/`stats` showing the canonical db path. A `db_path` of literal
`:memory:` (which `SqliteStore` opens in memory, `metadata.rs`) is passed through unchanged: no
`create_dir_all`, no canonicalisation. An in-memory `db_path` (`:memory:`, or any path containing
`mode=memory`, which `SqliteStore` also opens in memory) forces standalone mode, as
`--no-daemon` does, so each process keeps its own private DB as today; the admin guard skips
such a `db_path`.

The proxy records `canonicalize(current_exe())` once at startup and hashes that path plus its current mtime
every time it computes a key. It doesn't call `current_exe()` again later, because on Linux that
returns `... (deleted)` after a binary is replaced by rename. The daemon does the same with its
own canonicalised `current_exe()` at startup, which is the same path.

All daemons share one run directory, whatever their database. That lets the admin guard see
every live daemon:

```
<run_dir>                         ~/.ferrex/run, or FERREX_RUN_DIR (tests set this)
<run_dir>/                        mode 0700, created if missing, chmod 0700 if it exists
<run_dir>/<key>.sock              daemon socket
<run_dir>/<key>.pid               JSON: {"pid":123,"db_path":"/abs/ferrex.db"}
<run_dir>/<key>.lock              spawn lock (flock), left in place
<run_dir>/ferrexd.log             stderr of every daemon, appended
```

"Canonical db_path" is the absolute form `build_config` produces. The daemon and the admin
guard both use it.

The 0700 run directory makes the socket private without umask games. If the socket path
exceeds `sun_path` (103 usable bytes on macOS, 107 on Linux), the proxy fails with an error
naming the path. Test it with a `FERREX_RUN_DIR` nested deep enough that
`<run_dir>/<key>.sock` exceeds 107 bytes, so it fails on both platforms; a plain macOS tempdir
(`/var/folders/...`, about 81 bytes in total) must still fit.

The qdrant sidecar always lives in `~/.ferrex` on the configured `qdrant_port` (default 6334;
`MemoryService` starts it with
`base_dir = None`), and daemons for different DBs share it unless `--qdrant-url` is given. That
doesn't change. What does change is that daemons never stop it; see "Qdrant sidecar in daemon
mode".

- Pros: a rebuild or a config edit sends new sessions to a new daemon built from their own
  config, so a session can never run on another session's settings and never gets refused
  because of a mismatch. The old daemon idles out once its sessions close. Tests with different
  `--db-path` values never share a daemon.
- Cons: during an upgrade or after a config edit, two daemons are alive at once (double memory
  until the old one idles out) and both write the same SQLite file. That is today's situation
  with multiple sessions, so it's no worse than now.
- Why: the alternatives were (a) "first session wins", which breaks the first anti-goal, and
  (b) refusing mismatched sessions, which would make every new session fail after a
  `ferrex.toml` edit until all open sessions close.

### Decision: newline-framed forwarding with light JSON-RPC inspection (my call)

MCP over stdio is one JSON message per line, so the proxy frames on `\n` in both directions
from T1 onward. JSON-RPC batch arrays are out of scope: MCP 2025-06-18 removed them, and the
proxy forwards such a line untracked. It forwards whole lines only and never writes a partial line. For each line it
reads just enough to track ids:
- client → daemon: if the line has both `id` and `method`, record the id (as a
  `serde_json::Value`, since ids can be numbers or strings) with its `method` as in flight, and keep the latest
  `initialize` request (final once initialised) and `notifications/initialized` for replay (and, while not yet
  initialised, the kept first request for the inline lifecycle, see "Lifecycle modes");
- daemon → client: if the line has an `id` and no `method`, clear it from in flight. While not
  yet initialised, a `result` (not `error`) for a recorded method that counts marks the session
  initialised (see "Daemon loss and reconnect (proxy side)").

Daemon-initiated requests (lines with both `id` and `method` going daemon → client) aren't
tracked; ferrex sends none today.

The two directions run as independent tasks (or a `select!` where a pending write in one
direction never stops reads in the other), so a large `recall` result can't deadlock against a
small socket send buffer. If writing to stdout fails (EPIPE) the client is gone: exit 0, as on
stdin EOF.

- Pros: the reconnect logic in T3 needs exactly this, so T1 builds the loop once. The daemon still
  reuses the existing `FerrexServer` with rmcp `serve` over the accepted `UnixStream`. Under rmcp 3.5.1, `transport-io` enables
  `transport-async-rw`, which accepts either the stream itself or its `into_split()` halves
  (`transport/async_rw.rs`); it buffers reads itself, so no extra `BufReader` is needed.
- Cons: one JSON parse per message on the proxy, which is negligible next to the work behind it.

## Behaviour

### Start-up (proxy side, `ferrex` with no subcommand)

1. Build config with `cli::build_config`, compute the key and paths.
2. `connect_or_spawn`:
   1. Try to connect to `<key>.sock`. If that works, go to the hello.
   2. Otherwise create the run dir if needed (mode 0700, as the daemon does) and take an
      exclusive `flock` on `<key>.lock` (`std::fs::File::lock`, stable since
      Rust 1.89; poll `try_lock` from async code so the runtime isn't blocked). The kernel drops
      it if the holder dies, so there are no stale locks. The wait budget is 300 s.
   3. Under the lock, try to connect again. If it succeeds (another proxy spawned the daemon
      while we waited), release the lock and go to the hello on that connection. If it fails with `ENOENT` or
      `ECONNREFUSED`, remove the socket file and spawn. On `WouldBlock`/`EAGAIN` (Linux's answer for a
      full backlog), `EINTR` or `ETIMEDOUT` a live daemon is busy: never unlink. Release the lock and
      fail the attempt as retryable; the round's backoff provides the retry, so a wedged daemon
      ends the round after three attempts. Any other error (`EACCES`, ...) fails the attempt without unlinking.
      The PID file isn't consulted here. A daemon can't be in shutdown steps 2–4 (it holds the
      lock for those) while we hold the lock, and after a SIGKILL or a reboot a PID-based check could match an
      unrelated process that reused the PID and block the key for good. Spawn the daemon: the recorded exe path with the proxy's original arguments
      (`argv[1..]`), then
      `daemon --key <key>` appended (the global flags aren't `global = true` in clap, so they
      must come before the subcommand). Pass the same env. Spawn it detached in its own session
      with `setsid` (through `pre_exec`; a process group alone would leave it in the terminal's
      session, exposed to SIGHUP), with stdin/stdout `/dev/null` and stderr appended to `ferrexd.log`. Keep the
      `tokio::process::Child` (without `kill_on_drop`, since the daemon must outlive the
      proxy). It is reaped as described in step 4, so a daemon that dies doesn't sit as a
      zombie that `kill(pid, 0)` reports as alive.
   4. Poll until the socket accepts a connection, checking the child with `try_wait()` on each
      poll. Each poll tries the connect first and checks the child second, so a daemon that
      exited 0 because another one already serves the key isn't miscounted. If the child has
      exited and the connect still fails, release the lock and count a failure that points at
      `ferrexd.log`, rather than waiting out the budget. Ownership of the spawned child and the
      lock sits in a drop guard: if the `connect_or_spawn` future is dropped (stdin EOF during
      a `select!`, say) before the socket accepts, the guard kills the child and releases the
      lock, so a cancelled attempt doesn't leave a daemon that binds later (a proxy killed by a
      signal can; that daemon is reused by the next proxy or idles out). If the 30 s accept budget runs out,
      kill the child before releasing the lock, so it can't bind later and race the next
      spawn. The guard owns the `Child` until the socket accepts: it calls `try_wait` on each
      poll, and on timeout or drop calls `start_kill()` and hands the `Child` to a detached
      reaper task (`Drop` can't await). On success, disarm the guard, move the `Child` into
      a detached `tokio::spawn` that `wait()`s it, and keep that connection for
      the hello (don't drop it and reconnect), and release the lock right away, before sending the
      hello. The lock only covers the spawn, never the potentially long wait for init.
3. Send the hello on that connection and wait for the reply. The proxy's timeout is a fixed
   3600 s. What normally bounds the wait is the daemon's own init timeout, after which it
   replies `kind:"init"`. The proxy's timeout is only a backstop for a wedged daemon, and it
   doesn't depend on the proxy's own `FERREX_INIT_TIMEOUT`, which may differ from the env the
   daemon was spawned with. Today's stdio startup waits unbounded. A hello timeout on a
   connection that's still open (the daemon is alive but its init is stuck) ends the round
   instead of retrying, so one round never waits on a stuck daemon more than once.
4. On `ok`, run the forwarding loop until stdin hits EOF, then close the socket and exit 0.

During every wait (flock, accept poll, hello, replay, backoff) the proxy also reads stdin. Stdin
is read on a dedicated `std::thread` (blocking `read_line`) that sends lines and EOF into a
`tokio::sync::mpsc` channel, and every wait `select!`s on that channel. tokio's own `Stdin` does
an uncancellable blocking read, and dropping the runtime waits for it forever, so any exit not
caused by stdin EOF (startup failure, EPIPE, a failed round) would hang. As a backstop, every
proxy exit path flushes stdout and stderr and ends with `std::process::exit(code)`. If
stdin hits EOF, it gives up (running the guard's kill on a child it spawned whose socket isn't
accepting yet, as on the accept timeout, before `process::exit`, which skips destructors) and
exits 0, so it never outlives its Claude session. Lines read
before `ok` (Claude sends `initialize` straight away) aren't dropped. They're queued and tracked
exactly like forwarded lines: ids recorded as in flight, `initialize` and
`notifications/initialized` cached. The queue is flushed in order right after `ok`. So by the
time the forwarding loop starts, an `initialize` already counts as in flight for the reconnect
rules below.

If the connection hits EOF or an error before any hello reply arrives (for example it landed in
the backlog of a daemon that is shutting down), that counts as one failed attempt. Back off and
go round `connect_or_spawn` again. See "Failure counting" below.

The proxy must not load models, open SQLite or start qdrant. It writes nothing to stdout except
whole lines received from the daemon, plus the synthetic error responses and the
`notifications/tools/list_changed` described below. A single writer task (or a mutex) owns
stdout, so lines from these sources never interleave. Its
own diagnostics go to stderr.

### Handshake

One JSON line each way before any MCP traffic.

Proxy → daemon:
```json
{"ferrex_hello":1,"key":"3f9a0c1e7b2d4a55","namespace":"default"}
```
Daemon → proxy, one of:
```json
{"ok":true}
{"ok":false,"kind":"protocol","error":"..."}
{"ok":false,"kind":"key","error":"..."}
{"ok":false,"kind":"init","error":"..."}
{"ok":false,"kind":"session","error":"..."}
```

`kind:"session"` means this connection couldn't be set up, for example because
`ensure_collection` failed for its namespace (an invalid name, or qdrant briefly down). Only
that connection is refused. The daemon keeps running.

`ferrex_hello` is the protocol version. Daemon-wide settings are pinned by the key, so the hello
carries only the key, as a sanity check against a reused socket path, and the session's
namespace. The daemon answers only once `MemoryService` is ready and the session's namespace has
a collection (see below), so the reply also signals readiness.

### Daemon (`ferrex daemon --key <k>`, hidden subcommand)

- Startup: create the run dir. If `<key>.sock` exists and a connect to it succeeds, another
  daemon already serves this key, so exit 0 without touching anything. In practice the spawn
  lock makes this rare. Otherwise bind the socket (before any model loading, so the spawning
  proxy's accept poll succeeds within seconds), write the PID file, then start init. The daemon
  never unlinks a socket file at startup: the proxy already removed any stale one under the
  spawn lock (step 3), and an unlink here, outside the lock, could delete a racing daemon's
  socket. On `EADDRINUSE`, connect once more: if it succeeds another daemon won, so exit 0;
  if not, log "stale or busy socket, not unlinking" and exit 1. Any other bind or connect error
  is logged and exits 1. The proxy's accept poll sees the exit and points at `ferrexd.log`.
- Before init, the daemon sets `config.namespace` to `"default"`. The spawning session's
  namespace isn't part of the key and arrives in its hello like everyone else's, so an invalid
  namespace in one session gets `kind:"session"` for that connection instead of failing init for
  everyone. `from_config`'s own `ensure_collection(config.namespace)` then runs on `"default"`.
- Init runs on a dedicated `std::thread`, never on a runtime worker: the thread calls
  `Handle::block_on(MemoryService::from_config_with(..))` and sends the result over a
  `oneshot`. `Embedder::new` and `Reranker::new` are synchronous inside `from_config`, and
  `QdrantSidecar::start` sleeps with `std::thread::sleep` in its lock loop. qdrant-client's
  `build()` also blocks: unless `skip_compatibility_check()` is set it runs a health check on a
  thread it joins (`VectorStore::new`, and the sidecar's `health_check` today). A blocked worker
  stops driving tokio's I/O and timer drivers, and on a single-worker runtime (1 vCPU, or a
  cgroup quota of 1) that stalls accepts, hello reads and the init-deadline arm below. A plain
  thread rather than `spawn_blocking`, because a stuck load must not delay runtime shutdown.
  The requirement is that the accept loop keeps accepting and reading hellos during a long
  first-run download. No core change is needed for this. Hellos wait for init to finish.
- Init timeout: enforced from outside the init task, as a deadline arm in the accept loop's
  `select!`, because a `timeout` around the init future can't fire while a synchronous model
  load is stuck. `FERREX_INIT_TIMEOUT` seconds, default 1800, capped at 3000 so it stays under
  the proxy's 3600 s backstop. That's generous because a first run downloads ~1.5 GB of models.
  When it expires, init counts as failed, though the stuck load keeps its thread until the
  process exits.
- If init fails or times out, the daemon stays up in a refusing state: every waiting and later
  hello gets `kind:"init"` with the error, and the daemon shuts down 60 s after the failure. That
  deadline is fixed and new connections don't extend it. After it, the next proxy round spawns
  a fresh daemon, so a transient cause (qdrant briefly down, a contended sidecar lock) clears
  for sessions that are already initialised. A session that starts inside the window fails at
  startup with the init error, like the spawner, and needs `/mcp` reconnect (see known risks). Tests
  shorten it with `FERREX_TEST_INIT_FAIL_GRACE_MS`. Proxies get the real error instead of EOF, and they don't respawn a daemon that
  repeats the failing model load.
- `ferrex_embed`'s `ensure_cache_dir` calls `std::env::set_var`, which isn't safe once other
  threads are running. `main` calls it once, before subcommand dispatch and before any tokio
  runtime is built (and before `init_tracing()` if that can start a thread), so every mode is
  covered: daemon, proxy, `--no-daemon`, `diagnose`, `journal`, `audit`, `backfill`. Today
  `Embedder::new` and `Reranker::new` call it (back to back in `from_config`), after those modes have built a multi-thread runtime. Make it `pub` and re-export it from `ferrex-core` (the
  `pub use ferrex_embed::{...}` line in `crates/core/src/lib.rs`), since `ferrex-server` reaches
  `ferrex-embed` only through core.
- After init, the daemon calls `service.into_parts()`, as `run_server` does, and keeps the
  `QdrantSidecar` handle itself. The watcher thread (see "Qdrant sidecar in daemon mode") is
  started by `QdrantSidecar::start` in `sidecar.rs` under `LeaveRunning`, so it lives with the
  handle.
- For each connection: read the hello (timeout 10 s; a connection that sends nothing is closed
  and doesn't hold the count up), check version and key, call
  `vector_store.ensure_collection(namespace)` once per namespace for the life of the process
  (memoised on success only; concurrent hellos for the same namespace share one in-flight call;
  a failure replies `kind:"session"` and isn't remembered), reply `ok`, then run
  `FerrexServer::with_session(service.clone(), SessionDefaults { namespace })` over the socket
  halves. Each connection is its own rmcp session. Today every process runs
  `ensure_collection(config.namespace)` at startup, and `recall` on a missing collection errors
  in Qdrant. Doing it at hello time keeps `recall`-before-`store` in a new namespace returning
  empty results as it does now.
- Read the hello line without losing bytes that follow it. The proxy never sends MCP traffic
  before it gets `ok`; that's a protocol invariant. Reuse the same reader for rmcp, or read byte
  by byte up to `\n`.
- Idle exit: the daemon tracks two counts, connections in their hello phase and connections
  that passed the hello. The idle state is re-evaluated whenever init succeeds, a connection is accepted, passes
  its hello, fails its hello (refusal, 10 s hello timeout, EOF) or ends. The timer is armed
  exactly when init has succeeded and both counts are 0, and disarmed otherwise. Accepting a
  connection suspends the current deadline rather than discarding it. If every connection since
  then failed its hello, re-arming restores that deadline (firing at once if it has passed);
  otherwise, as after init, after a session that passed its hello ends, or after an idle exit
  abandoned on a busy lock, arming starts a fresh `FERREX_IDLE_TIMEOUT` countdown. A daemon whose
  spawner died before connecting therefore exits one timeout after init, and a connection that
  fails its hello can't leave the timer disarmed. Timeout `FERREX_IDLE_TIMEOUT` seconds,
  default 600.
- Numeric env settings (`FERREX_IDLE_TIMEOUT`, `FERREX_INIT_TIMEOUT`) fall back to their
  defaults when unset (silently) or when zero or unparsable (with a warning in the log naming
  the variable and the rejected value).
- The accept loop owns the listener and is the only place shutdown runs: one `select!` over
  accept, signals, the init result (`oneshot`), the init deadline, the init-failed grace
  deadline, the idle timer, and a channel on which connection tasks report hello passed, hello
  failed and connection ended (each re-evaluates the idle state). So "drain the listener"
  below needs no coordination with another task.
- Shutdown (idle timer, SIGTERM, SIGINT, end of the init-failed grace period). A signal during
  init doesn't wait for it. With `LeaveRunning`, a half-started qdrant is just left running and
  reused later. The only leftover is the sidecar's `create_new` lock, if the signal lands inside
  `start`'s short critical section (see known risks). The synchronous model loads can't be
  interrupted, so the process ends with `std::process::exit` after the steps below rather than
  by dropping the runtime, which would wait for a stuck init thread. Then:
  1. Take the spawn `flock` on `<key>.lock` (polling `try_lock` or via `spawn_blocking`, never a
     blocking call on a runtime thread), waiting at most 10 s. For an idle exit, if the lock
     isn't free by then, keep serving and re-arm the timer. For the other triggers, skip steps
     2–4 (leaving the files for the next spawner's stale-socket cleanup) and go to step 5.
  2. Drain the listener with non-blocking accepts. For an idle exit, if that produced a
     connection, or any connection is in its hello phase, or the count is no longer 0, release the lock, disarm and keep serving.
     Otherwise close the listener. For the other triggers, connections drained here and any
     still in their hello phase are closed. A connection that gets EOF before its hello reply,
     including one that lands in the backlog after this, is a retryable failure for the proxy.
     So a proxy that would otherwise have got `kind:"init"` at the end of the grace period
     respawns once.
  3. Remove `<key>.sock` and `<key>.pid`, but only if they are still ours: the PID file's `pid`
     is ours, and the socket's dev/ino match what we bound. A PID file that can't be parsed is
     left in place, since it can't be shown to be ours.
  4. Release the lock.
  5. Cancel the running rmcp sessions, `service.shutdown()` if init succeeded (with a 10 s
     timeout), then `std::process::exit(0)`. The sidecar handle is `LeaveRunning`, so qdrant
     keeps running.

  Because the files are removed under the spawn lock and before the slow teardown, a proxy that
  spawns a new daemon in the meantime can't have that daemon's socket deleted by the old one.
- Logs go to stderr, which points at `ferrexd.log`. The plain stderr branch of `init_tracing`
  sets `.with_ansi(std::io::stderr().is_terminal())`, since tracing-subscriber 0.3 only honours
  `NO_COLOR` and would otherwise write colour codes into the log. `FERREX_LOG`, `FERREX_LOG_FILE` and
  `FERREX_LOG_FORMAT` still apply. Only tracing output follows `FERREX_LOG_FILE`; fatal errors returned
  from `main` always reach `ferrexd.log`, and when it is set the proxy's failure messages also
  name it. Process-level env such as these, `FERREX_IDLE_TIMEOUT`,
  `FERREX_RUN_DIR` and the ORT/HF cache dirs come from whichever proxy spawned the daemon and
  aren't part of the key. That's acceptable since none of them change results. The daemon
  inherits the proxy's cwd and must not `chdir`, although paths in the config are already
  absolute.

### Qdrant sidecar in daemon mode

Today the process that spawned qdrant kills it on exit (`QdrantSidecar::shutdown` and `Drop`),
even if other processes are still using it. With daemons, two overlapping is routine (after
every rebuild or `ferrex.toml` edit), so the old daemon idling out would take qdrant away from
the new one. The new daemon would then be broken for as long as any session holds it open,
which breaks the third anti-goal.

So in daemon mode, ferrex never stops qdrant on exit:

- `QdrantSidecar::start` takes a policy: `KillOnDrop` (today's behaviour) or `LeaveRunning`.
  `shutdown` and `Drop` respect it. `LeaveRunning` leaves `qdrant.pid` in place so the next
  daemon reuses qdrant through the existing PID-file logic. `MemoryService` gets a constructor
  that passes the policy through, e.g. `from_config_with(config, SidecarPolicy::LeaveRunning)`,
  so a `from_config` that fails after starting the sidecar (say, an embedding-model mismatch)
  doesn't kill it either. The daemon uses `LeaveRunning`. Standalone mode and the CLI commands
  keep `KillOnDrop`.
- Supervision. With `LeaveRunning`, if this process spawned qdrant, it hands the `Child` to a
  watcher thread. The thread calls `wait()`, so a crashed qdrant is reaped at once instead of
  lingering as a zombie that `kill -0` reports as alive to every other daemon. If qdrant exits
  while the daemon is still running, the watcher calls `QdrantSidecar::start` again (the
  existing code, with its existing short lock), and logs each restart. Restart budget: a start
  attempt that spawns qdrant, or fails, uses one unit; one that only reuses a qdrant another
  process spawned uses none. At most 3 units in a sliding 10-minute window. When the budget is
  used up, the watcher logs it, keeps watching, and retries once the oldest unit is more than
  10 minutes old. The watcher runs an inner start function (async) through a runtime `Handle` it
  captured, with `block_on`. The inner function does what `start` does today but returns the
  `Child` without starting a watcher of its own. If it spawned a new `Child`, the same loop
  watches it, sharing the restart budget. If it instead reused a qdrant someone else spawned, the
  watcher switches to polling (below). Clients in every daemon reconnect to the same port on
  their own. When the daemon exits, the watcher is simply abandoned.
- A daemon that only reused qdrant (it didn't spawn it) has no `Child`, so its watcher thread
  polls the reused PID with `pid_alive` every 2 s instead of calling `wait()`. When the PID is
  gone, it runs the same inner start under the sidecar lock, with the same restart
  budget. If another daemon got there first, the inner start reuses the new qdrant and the
  watcher polls its PID. If it spawns qdrant itself, it now owns the `Child` and switches to
  `wait()`. So a qdrant that dies after its owner has exited is respawned by whichever live
  daemon notices first, and the sidecar lock keeps it to one respawn. A reused qdrant can be a
  zombie: a `KillOnDrop` owner (say, a long-lived `--no-daemon` session) only reaps its child in
  `shutdown()`, so a qdrant that crashes under it stays a zombie until that session exits.
  `pid_alive` reports zombies as dead (see below), so the poller respawns qdrant as for any
  other death. For the same reason, `shutdown()` under `KillOnDrop` removes `qdrant.pid` and
  `qdrant.port` only if `qdrant.pid` still names its own child's PID: by then a daemon may
  have respawned qdrant and written its own PID there, and today's `shutdown()`
  (`sidecar.rs:138-145`), which removes `qdrant.pid` without checking which PID it names,
  would orphan it. After killing and reaping the child, it takes the sidecar lock
  (`acquire_lock_file`) and does the compare and remove while holding it, so a respawner can't
  write its PID between the compare and the unlink. If the lock can't be taken within its usual
  budget, both files are left in place: a dead PID is already handled as stale by the next
  `start`. Removing the files before the kill isn't an option, because a starter that sees no
  PID file while the old qdrant still holds the port would pass its health check against a
  process that is about to die.
- Error path. If a `start` (initial or a watcher restart) spawns qdrant and the health check
  then fails, it kills and reaps that `Child`, then, under the sidecar lock as in `shutdown()`,
  removes `qdrant.pid` and `qdrant.port` if `qdrant.pid` still names that child's PID, before
  returning the error. So a failed qdrant
  never lingers as a zombie that other daemons would try to reuse, and under `LeaveRunning`
  (whose `Drop` leaves the PID file alone) no PID file is left naming a dead process that the
  OS may later hand to something else. A spawned child that exits during its own health check
  is a failure too, even if the port answers: the answer came from someone else's qdrant (say,
  one started through a wrapper that a probe misjudged), and treating it as success would leave
  `qdrant.pid` naming the dead child and the live qdrant unrecorded, with a `LeaveRunning`
  watcher respawning into the bound port every time its budget allows. The health-check loop
  checks the child with `try_wait()` on each poll and returns this error, through the same
  cleanup.
- Identity check on reuse. With `LeaveRunning`, `qdrant.pid` deliberately outlives the process,
  and it survives reboots, so the PID it names may belong to an unrelated process later. Before
  the reuse path trusts a live PID, `start` checks the process really is qdrant with a shared
  helper, `is_qdrant(pid, qdrant_bin)`, which `nuke` uses too. It reads the process name the
  same way `pid_alive` avoids a `ps` on `PATH`: `/proc/<pid>/comm` on Linux, `/bin/ps -o comm= -p
  <pid>` on macOS. If the name can't be read (no `/proc` entry, `/bin/ps` missing, non-zero exit
  or empty output), that counts as a name mismatch and goes to the probe below. The name check
  itself is a pure `matches_qdrant_name(comm, qdrant_bin)`, which compares the basename of the
  name with the basename of `qdrant_bin` on their first 15 characters (macOS prints a full path,
  Linux truncates to 15). Nix wrappers are normalised: a leading `.` and a
  trailing `-wrapped` are stripped from the process name. A name mismatch alone doesn't make the
  PID file stale, since `qdrant_bin` may be a wrapper script whose process name is `sh`. On a
  mismatch, `start` runs one health-check probe (the existing 5 s client timeout, no retries;
  the probe client, like `health_check`'s, is built with `.skip_compatibility_check()`, since
  qdrant-client 1.19's `build()` otherwise runs its own health check first and `println!`s to
  stdout when it fails, which is the MCP stream in `--no-daemon` mode)
  against the recorded port (below), or the configured port when none is recorded. If it
  answers, the process is treated as qdrant. If not, the PID file is stale: remove it and spawn
  a fresh qdrant, as for a dead PID. The probe runs after the sidecar lock is released, because
  other starters give up on the lock after ~2 s (`acquire_lock_file`: 10 tries 200 ms apart)
  and the probe can take 5 s. If the probe fails, `start` takes the lock again and re-reads
  `qdrant.pid`: if it still names the same PID, it removes it and spawns; if it changed or
  is gone, another starter has already acted, and `start` begins its reuse check again from what
  it just read (a missing file spawns a fresh qdrant, as on a first start).
  A name match needs no probe, so the usual reuse path holds the lock as briefly as today.
  Exception: if `qdrant.pid` was modified less than `HEALTH_CHECK_MAX_TIMEOUT` (30 s) ago, the
  spawner may still be inside its own post-spawn health check (it writes the PID file before
  qdrant listens), so a failed single probe proves nothing. In that case `start` runs the full
  `health_check` (30 s budget, also outside the lock) instead of the single probe, and treats
  the PID file as stale only if that fails too. Only a name mismatch takes this path, so with
  the default binary, or a wrapper that `exec`s, no starter ever waits on it.
- Port check on reuse. Whoever spawns qdrant also writes `<pid> <port>` (its gRPC port) to
  `qdrant.port` next to `qdrant.pid`, right after it and before the sidecar lock is released. That's a separate file so that `qdrant.pid` keeps its bare-PID format, which
  older ferrex binaries still parse. Before reusing a live qdrant, `start` compares
  `qdrant.port` with the configured `qdrant_port`. On a mismatch it fails at once with
  `qdrant sidecar (pid N) is running on port X, but qdrant_port is Y. Set qdrant_port = X, or
  close the sessions still using the old config (or kill their ferrex daemons), then stop
  qdrant`, instead of health-checking the wrong port for 30 s. `qdrant.port` is trusted only
  when its PID equals the one in `qdrant.pid`; a missing file, or one naming another PID (left
  by an older binary, which can't clean it up, or by a crash), skips the check. Every path
  that removes `qdrant.pid` (stale-PID cleanup, `KillOnDrop` shutdown, the spawn error path)
  removes `qdrant.port` with it, and `nuke` deletes it with the other qdrant files.
- Beyond the above and the `is_process_alive` → `pid_alive` rewrite (see dependencies), nothing
  else in `sidecar.rs` changes. The lock mechanism is unchanged. The health check changes only by
  `.skip_compatibility_check()` on its client and the per-poll `try_wait()` on a spawned child. A
  live PID that is qdrant but fails the health check is still an error, as today.

Decision (my call):
- Pros: small change to existing code, no cross-process lock or kill-and-wait logic. CLI and
  `--no-daemon` users of the same qdrant are never cut off by a daemon. A crashed qdrant is
  healed by whichever live daemon notices first, its owner or a reuser.
- Cons: qdrant stays resident after the last session closes, typically tens to low hundreds of
  MB against the ~2 GB per session this work removes. Some rarer qdrant failures aren't healed
  (see known risks).
- Why: "last user out stops it" and full healing need every qdrant user, CLI and standalone
  included, to register, plus a real flock and identity-checked kills. That's a lot of
  machinery for the qdrant footprint. It's listed as a follow-up.

### `nuke` and a running qdrant

`nuke` deletes `qdrant-data`, `qdrant.pid`, `qdrant.lock` and `qdrant-config.yaml` under
`db_path.parent()`, and today relies on qdrant having died with the session that spawned it.
With `LeaveRunning`, qdrant is usually still up, and deleting its files under it would leave an
untracked orphan on the port. So:

- The rule hinges on one question: are the qdrant files `nuke` will delete the live sidecar's
  files? They are exactly when `db_path.parent()` is `~/.ferrex`, the sidecar's real directory
  (`from_config` always passes `base_dir = None`; both sides canonicalised before comparing).
  `--qdrant-url` doesn't change the answer: the files are deleted either way, and other daemons
  may be using that sidecar.
- Sidecar files: `nuke` refuses while any live daemon exists in the run dir (`any_live`),
  whatever DB it serves, because any of them may be using the sidecar; `any_live` can't tell
  which ones run on `--qdrant-url`, so those block it too (deliberately conservative). Then it
  stops qdrant. Before
  signalling it checks the PID really is qdrant with the same identity check the sidecar's
  reuse path uses (`is_qdrant`, falling back to the one health-check probe). `nuke` is
  synchronous, so on an `is_qdrant` mismatch it runs `runtime()?.block_on(probe_qdrant(port))`,
  with the port from `qdrant.port` when its PID matches `qdrant.pid`, else the configured
  `qdrant_port`, as on the reuse path. If it isn't,
  `nuke` skips the kill and says so. Otherwise SIGTERM, poll until gone (up to 10 s), then
  SIGKILL.
- Custom `--db-path` outside `~/.ferrex`: the qdrant files next to that DB belong to no
  sidecar, so `nuke` uses the same-DB guard the other write commands use (`live_for_db`) and
  leaves the shared qdrant alone.
- Then it deletes files as today, plus `qdrant.port`.

This is part of T4.

### Per-session default namespace

Every MCP tool must behave as if `config.namespace` were the session's namespace, which is what
happens today because each session has its own process. The tools that fall back to the
default are `store`, `recall`, `timeline` and `taxonomy`. `reflect` takes a required namespace and `forget`
takes none. `stats` takes a required namespace too, but `stats` with `diagnostics: true` calls
`MemoryService::diagnostics()`, which uses `config.namespace` for the entity count and the
reported collection (`crates/core/src/service/stats.rs`).

- `store`, `recall`, `timeline`: the server fills in the session namespace when the param is
  `None`. Collections are already ensured at hello time, so `store.rs` needs no change, and an
  explicit namespace param keeps today's behaviour.
- `taxonomy`: today, no namespace means "counts for `config.namespace` plus the list of all
  namespaces". Injecting the namespace in the server would drop the list. Core needs to take the
  fallback namespace separately from the "was a scope given" flag, e.g. a
  `default_namespace: Option<String>` on `TaxonomyRequest` that the server sets from the
  session. The builder picks the exact shape. The tool description and output must not change.
- `stats` diagnostics: `diagnostics()` takes the namespace as an argument. Core `stats()`
  (`stats.rs:74`, where it's called today) passes the request's namespace, and the CLI
  `diagnose` passes `config.namespace`. This changes the output: today the diagnostics ignore
  the request's namespace, while the rest of `stats` is scoped to it. With the request's
  namespace, `entity_count` and `collection` follow the requested namespace, while
  `memory_count`, SQLite size, pending ops, cache and recent ops stay database-wide as today.
  This is listed as the one exception under Goals.
- `--no-daemon` builds its server with `with_session` too, passing `config.namespace`, so there's
  one server constructor.
- In daemon mode `crates/core/src/service/store.rs:25-26` calls `ensure_collection` (check-then-
  create: `collection_exists`, then `collection_info` if it exists or `create_collection` if not)
  on every store whose namespace isn't `"default"`, since the daemon's config namespace is pinned.
  `pipeline/resolve_entities.rs:18` repeats it when the request has entities (existing
  duplication). For a session namespace the hello memo has already created the collection, so
  the call takes the cheap exists path, and it stays. For an explicit, brand-new namespace param
  two concurrent stores can both see it missing and the second `create_collection` errors; that
  race exists today within one process, and the daemon only adds more concurrent clients.
- The CLI-only `admin.rs` reconcile runs in its own process with its own config and keeps using
  `config.namespace`.

### The proxy never exits once the client is initialised

Claude Code doesn't respawn a stdio MCP server that has exited. It shows the server as failed
until the user runs `/mcp` reconnect or restarts the session. So once the client's `initialize`
has completed, the proxy stays alive until stdin hits EOF, whatever happens to the daemon. Each
request it can't deliver gets a JSON-RPC error response, and the next client request triggers
another reconnect attempt.

Before the first `initialize` completes, exiting non-zero with the error on stderr is fine.
That's what ferrex does today when startup fails, and Claude shows it as a failed server.

### Daemon loss and reconnect (proxy side)

"Initialised" means the first of these has been forwarded to the client: the daemon's
successful (non-error) response to the client's `initialize`, or, for a client that never sends
`initialize`, a successful response to any request other than `ping` or `server/discover`. An
error response leaves the session uninitialised. The proxy caches the client's `initialize`
request, and its `notifications/initialized` once the client sends it. While uninitialised, each
new `initialize` replaces the cached one (as with the inline kept line below), so a retry after an
error response is what gets replayed; the cached line becomes final once its own successful
response initialises the session.

Lifecycle modes. rmcp 3.5.1 also supports MCP 2026-07-28, which drops the `initialize`
handshake: protocol info travels in each request's `_meta`, and the server picks the inline
lifecycle when the first non-`ping`/`server/discover` request carries complete `_meta` (with any
supported version, not only 2026-07-28; `rmcp` `service/server.rs`). In that mode there is no
handshake to replay once the session is initialised: after reconnect and hello, the proxy
flushes the queue directly, and the first queued request starts the new daemon's session. Until
the session is initialised, the proxy keeps the full line of the first request other than `ping`
or `server/discover` (counting `initialize`). rmcp answers a pre-lifecycle request with
incomplete `_meta` or an unsupported version with an error and keeps waiting
(`service/server.rs`), so when the kept request gets an error response, the next such request
the client sends replaces it. It isn't replaced on every send, since a client may pipeline and
the first request is the one that starts the session. The kept line is dropped once
initialised. If the kept line is an `initialize`, the "initialize in flight" case applies.
Otherwise the "initialize in flight" case below becomes "first request in flight" (the kept
line's id is still in flight; if it isn't, "not in flight" applies): reconnect at once and
re-send that kept line. This is the one place the proxy re-sends a request that may already have
committed. The kept request is the only one the client has made so far, and answering it
`-32603` would surface a startup failure to a client that has never seen a working session. A
re-sent `store` that already committed isn't written twice, but the client gets an error for it
(see below), and a duplicate `forget` of the same ids finds nothing left to delete. Unlike a re-sent `initialize`, the proxy doesn't hold the queue behind
it, since the first request may be a long tool call: once the hello succeeds and the kept line
is written, the attempt is over, the queue is flushed in order (so a `notifications/cancelled`
for the kept id gets through) and forwarding resumes. The proxy marks the kept id "re-send
pending" until its response arrives, and the round stays open while it is (see "Failure
counting"). Becoming initialised (by any successful response to a request other than `ping` or
`server/discover`, including one to a later request) ends the
pending state and the round: the kept line is already dropped, and a later daemon loss follows
the initialised rules below, with the kept id answered with the `-32603` error like any other
in-flight id. If the daemon is lost again while the kept id is pending, that counts as the
failure of the attempt that re-sent it, against the same three (the original loss isn't itself
an attempt): answer every other in-flight id with the
`-32603` error below, reconnect at once and re-send only the kept line. If the round fails,
exit non-zero as at startup. `list_changed` after a
key change applies in both modes.

When the socket hits EOF or an error while stdin is still open:

- **Not yet initialised, `initialize` in flight** (the daemon died between the client's
  `initialize` and its response): this is still startup. Reconnect at once, because the client
  is waiting and won't send another line. Answer every other in-flight id (a `ping`, say) with
  the `-32603` error below and clear it; don't re-send those, since a non-idempotent request may
  already have committed. Re-send the cached `initialize` (the one in flight, per the replacement rule above) and forward its response. If that
  round fails, exit non-zero as at startup. The inline lifecycle's "first request in flight"
  case does the same with the other in-flight ids, but re-sends the kept line despite the rule
  above, for the reason given in "Lifecycle modes".
- **Not yet initialised, `initialize` (inline lifecycle: the first request) not in flight**
  (the daemon died before the client's first line, or with only a `ping` outstanding, or after
  `initialize` got an error response): answer any in-flight ids with the `-32603` error below,
  then wait for the client's next request, reconnect, and forward it. Notifications that arrive
  before then are dropped, except `notifications/initialized`, which is cached as above: before a
  lifecycle is chosen, rmcp closes a connection whose first message is neither a request nor a
  `ping`, in either lifecycle. A failed round exits non-zero as at startup.
- **Initialised:**
  1. Answer each in-flight request with a JSON-RPC error (`-32603`, message
     `ferrex daemon restarted, retry`).
  2. Don't reconnect yet. Wait for the next client request, then reconnect. So a deliberate
     SIGTERM or `pkill` really stops the daemon until someone uses ferrex again. Client
     notifications that arrive while disconnected are dropped (they refer to the old session's
     ids, and in the inline lifecycle rmcp closes a connection whose first message isn't a
     request), except `notifications/initialized`, which is cached as above. "Disconnected"
     lasts from the loss until the next client request; "reconnecting" lasts from that request
     until the queue is flushed. Lines that arrive while reconnecting are queued in order,
     except `notifications/initialized`, which is only cached, never queued: replay sends it if
     it was cached before replay starts, and one arriving after that is forwarded normally.
  3. Reconnect: recompute the key and paths from the recorded exe path's current mtime and the
     current config (after a rebuild in place this reaches or spawns a daemon running the new
     code, as long as the key derivation and CLI flags stay compatible with the old proxy; see
     "Failure counting" for when they don't), then `connect_or_spawn` and the hello. If the recorded exe path no longer exists or
     the config can't be built, that counts as a failure (below).
  4. Replay: send the cached `initialize` (its id isn't recorded as in flight, and its dropped
     response doesn't clear a client request that happens to reuse that id), wait for its response (timeout 30 s, since the hello
     reply already means the daemon is ready) and drop it. Then send the cached
     `notifications/initialized` if the client has sent one; if it hasn't yet, it'll be
     forwarded normally when it arrives. If the new daemon's key differs from the old one (a
     rebuild or config edit), also send the client `notifications/tools/list_changed`, so it
     refetches the tool list. For the client to honour it, `FerrexServer` advertises the
     capability. That means writing `get_info` by hand instead of letting `#[tool_handler]`
     generate it: `ServerCapabilities::builder().enable_tools().enable_tool_list_changed()`
     (rmcp 3.5.1 has it), with server info `Implementation::new("ferrex", env!("CARGO_PKG_VERSION"))`.
The macro's `Implementation::from_build_env()` expands `env!` inside rmcp, so today ferrex
reports itself as `rmcp`/`3.5.1` (`rmcp-macros-3.5.1/src/tool_handler.rs:151`); reporting its
own name and version is a deliberate change, and nothing depends on the old value. That edit
     is in `server.rs`, so it belongs to T2. Flush the queue in order and resume forwarding. EOF
     or an error response during replay counts as a retryable failed attempt; a replay timeout
     on a still-open connection ends the round (see "Failure counting").

If stdin hits EOF while a reconnect is pending, drop the queued lines and exit 0.

Ids of requests the client cancelled (`notifications/cancelled`) may stay in flight if the daemon
never answers them. On daemon loss they get a `-32603` the client ignores, which is harmless.

A `store` that was in flight when the daemon died may already have been committed. If it was and
the client retries it, the retry isn't written again, but the client gets an error for a store
that succeeded: episodic and procedural copies are rejected by dedup, semantic copies by the
conflict stage, both as `invalid_params` with `code: "duplicate"` and the existing id. A
`supersedes` store skips both stages and fails with "supersedes target ... is already
invalidated". If the store was still pending in the journal when the daemon died, the new
daemon's `recover_on_startup` compensates it, so the retry writes it fresh and succeeds. Nothing
is lost or duplicated, so this is acceptable.

### Failure counting

One connection attempt is `connect_or_spawn`, the hello and then, depending on state: when
reconnecting an initialised session, the dropped `initialize` replay; when reconnecting with the
client's `initialize` in flight, the forwarded re-send of that line up to its response; in
the inline lifecycle with the first request in flight, the re-send of the kept line up to
writing it. EOF before the `initialize` response, or before the kept line's response while it's
"re-send pending", is a retryable failed attempt in the same round, so a daemon that dies on the
re-sent line is respawned at most three times before the round fails. It fails on: a connect under the lock that fails with
`EAGAIN`/`EINTR`/`ETIMEDOUT` (busy or wedged daemon), any `ok:false`, EOF or an error before the
reply, a timeout, the spawned child exiting early, or the key not being computable (exe path
missing, config error).

Budgets: flock wait 300 s; accept poll after spawning 30 s (the daemon binds before loading
models); hello reply 3600 s (backstop); replay response 30 s; forwarded `initialize` re-send
30 s; forwarded inline first request: no proxy timeout (it may be a long tool call; the client's
own request timeout bounds it, and later lines aren't queued behind it).

- A round is up to three attempts, with 0.5 s and then 1 s of backoff between them. Each round
  starts with a fresh count; a successful attempt ends the round, except in the inline
  lifecycle's "first request in flight" case: there the round stays open while the kept id is
  "re-send pending" and ends when its response arrives or the session becomes initialised.
- `kind:"protocol"`, `kind:"init"`, a flock wait timeout, and a hello, replay or
  forwarded-`initialize` timeout on a still-open connection end the round at once, because
  retrying would reach the same stuck daemon or lock holder. Everything else is retryable, including a spawned child
  that exits early (that's how a key mismatch after a config edit recovers).
- At startup (before the client's `initialize` has completed), a failed round makes the proxy
  exit non-zero with the last error on stderr.
- After that, a failed round answers the queued and pending requests with a JSON-RPC error whose
  message carries the cause. For `kind:"protocol"`, and when the recorded exe path no longer
  exists (a Nix store path garbage-collected after an upgrade), the message tells the user to
  run `/mcp` reconnect so Claude starts the new binary. The same hint is added when the round's
  failures include a spawned child exiting early and the recorded exe's mtime differs from the
  one at proxy startup: an in-place rebuild that changed the key derivation or CLI flags makes
  the new daemon reject the old proxy's key (or argv) on every attempt. The proxy stays up,
  and the next client request starts a new round.

A request that deterministically crashes the daemon will restart it each time a client retries
it. That costs a model load per retry, and it's visible in `ferrexd.log`. Good enough.

### Standalone mode

`--no-daemon` (env `FERREX_NO_DAEMON=1`) runs today's in-process stdio server. The flag needs
`value_parser = clap::builder::BoolishValueParser::new()`: a plain `bool` flag with `env` only
accepts `true`/`false` from the environment (clap 4.6), so `FERREX_NO_DAEMON=1` would fail to
parse. A cli test checks that `=1` selects standalone mode and `=0` doesn't, and that
`--db-path :memory:` selects standalone mode without the flag. It's unchanged
except that it builds the server with
`FerrexServer::with_session(.., SessionDefaults { namespace: config.namespace })` (see the
namespace section). It's
used for debugging, it's the escape hatch if the daemon misbehaves, and the existing
`crates/server/tests/mcp_protocol.rs` switches to it. The existing `run_server` becomes this path.

### Admin subcommands

`audit reconcile --fix` and `backfill` without `--dry-run` change data a daemon may be holding.
If any live daemon is using the same database, they refuse. `nuke` uses the rule from
"`nuke` and a running qdrant": when it would delete the sidecar's files it refuses while any
live daemon exists, and otherwise it uses the same-DB rule. "Live" means some
`<run_dir>/*.pid` whose `db_path` equals this command's canonical `db_path`, whose `pid` passes
`ferrex_store::pid_alive`, and whose `<key>.sock` accepts a connection. A PID file that can't be read or parsed is skipped as
not live (logged at debug) and never fails the command. Requiring the socket too means a
stale PID file whose PID was reused by an unrelated process doesn't block the command forever.
`any_live` (for `nuke` on the sidecar's directory) applies the pid and socket checks to every PID file, ignoring `db_path`. The key isn't used for this, so a dev binary or an old
daemon left over from an upgrade is still found. The refusal tells the user to close their
Claude sessions or wait for the idle exit. Read-only commands (`diagnose`, `journal show`,
`audit reconcile` without `--fix`, `backfill --dry-run`) keep running directly as they do now.
They aren't strictly read-only: building a `MemoryService` runs `recover_on_startup`, which can
compensate ops the live daemon has in flight, and prunes old `completed_ops` history (harmless,
since every startup does the same). That's today's behaviour with multiple sessions
too, and it's listed under known risks.

The guard's "socket accepts" probe opens a connection that never sends a hello. Accepting it
suspends the idle timer, and when it closes (or hits the 10 s hello timeout) the previous
deadline is restored, so a probe never postpones idle exit; one stuck in its hello delays it
by at most the 10 s hello timeout.

This guard is best-effort: a session that starts while `nuke` is running, or a reused PID, can
slip through. The guard is there to catch the common mistake, not to lock the database.

### Tests and daemons

T1 creates the shared harness in `crates/server/tests/common/mod.rs`:
- a temp DB dir and a temp `FERREX_RUN_DIR`, an external qdrant via `--qdrant-url`, and
  `FERREX_IDLE_TIMEOUT` of a few seconds;
- a guard whose `Drop` kills the daemon via its PID file even when a test panics;
- a fake daemon: a small in-test unix listener that speaks the hello and then follows a script
  (reply `ok:false` with a given kind, close mid-request, delay a response, echo). Proxy
  behaviour that depends on timing is tested against the fake daemon, not the real one, so
  "a request in flight when the daemon dies" is deterministic.

`ferrex-server` is a binary-only crate, so integration tests can't call `DaemonPaths` directly.
T1 adds a hidden subcommand, `ferrex daemon-paths`, which takes the same flags and env as the
proxy and prints the key and socket path as JSON. Tests use it (through `NEXTEST_BIN_EXE_ferrex`
/ `CARGO_BIN_EXE_ferrex`, as `mcp_protocol.rs` already does) to find where the fake daemon must
listen. Unit tests inside `daemon/*.rs` cover the pure parts (key derivation, hello
encode/decode).

Each task adds its own test file on top: T1 `daemon_e2e.rs` and `proxy_fake.rs`, T2
`daemon_namespace.rs`, T3 `daemon_reconnect.rs`, T4 `daemon_admin_guard.rs`.

New dependencies: `sha2` for the key, and `rustix` (already in `Cargo.lock` transitively, but
only with `fs`) for `setsid` and signals. Declare `rustix = { version = "1", features =
["process"] }` in `[workspace.dependencies]` and use `rustix.workspace = true` in ferrex-store
and ferrex-server; `process` covers `setsid`, `kill_process`, `test_kill_process`, `Pid` and
`Signal`.
`sidecar.rs`'s private `is_process_alive`, which shells out to `kill -0`, becomes
`pub fn pid_alive(pid: u32) -> bool` in ferrex-store, implemented with `kill(pid, 0)` through
rustix. It converts with `i32::try_from` and `Pid::from_raw` (no `as i32`: rustix only
`debug_assert`s on negatives, and a negative pid signals a process group), so zero or
out-of-range values read as not alive and are never signalled; `EPERM` counts as alive, only
`ESRCH` as dead. `sidecar` is a private module, so `crates/store/src/lib.rs` re-exports what
other crates need: `pub use sidecar::{QdrantSidecar, SidecarPolicy, pid_alive, is_qdrant, probe_qdrant};`, where
`probe_qdrant(port) -> bool` is the async one-try identity probe (5 s, `skip_compatibility_check`).
When `kill` succeeds, it also checks for a zombie, which `kill` reports as
alive: on Linux it reads `/proc/<pid>/stat` and treats state `Z` (the first field after the last
`)`) as dead; on macOS it runs `/bin/ps -o stat= -p <pid>` (an absolute path, since a nix
build's `PATH` has no `ps`) and treats a state starting with `Z` as dead. If the check can't run
or returns nothing, the `kill` result stands. Name matching can't stand in for this: on Linux
`/proc/<pid>/comm` still reads `qdrant` for a zombie, so `is_qdrant` would accept it; on macOS
`ps -o comm=` prints `<defunct>`, which only sends `is_qdrant` callers to the probe fallback.
Every caller must therefore check `pid_alive` before `is_qdrant`. The sidecar reuse path, the admin guard, `any_live` and `nuke`'s poll-until-gone all
use it, and `nuke`'s SIGTERM/SIGKILL go through rustix too. Use std's `MetadataExt` for the socket dev/ino check.

Until T3 lands, a T1 proxy that loses its daemon after startup writes the error to stderr and
exits non-zero. T3 replaces that with the reconnect behaviour.

Test hooks. Integration tests drive the real binary, so a few env-gated hooks make the
timing-dependent cases deterministic. They're read only when set, cost nothing otherwise, and
are documented as test-only:
- `FERREX_TEST_DAEMON_EXIT_BEFORE_BIND=1`: the daemon exits with an error right after parsing
  its args. The proxy's "spawned child exited early" path is tested with this.
- `FERREX_TEST_INIT_FAIL_GRACE_MS=<n>`: shortens the 60 s init-failed grace period.
- `FERREX_TEST_INIT_DELAY_MS=<n>`: init sleeps `n` ms before loading anything, to exercise the
  init timeout without depending on network speed.
- `FERREX_TEST_IDLE_PAUSE_MS=<n>`: after the idle timer fires, the accept loop sleeps `n` ms
  before draining. A test connects during the pause and checks it gets served.
- `FERREX_TEST_PROXY_TIMEOUT_MS=<n>`: the proxy uses `n` ms for its flock wait, hello, replay,
  forwarded-`initialize` and accept budgets, so the round-ending timeout rules can be tested
  against a fake daemon that stalls.

The attempt loop. `spawn` exposes the round logic so T1 and T3 share it:
`run_round(resolve, attempt, used: u8) -> Result<Connected, RoundError>`. `resolve` is called at the start
of every attempt and rebuilds the config and `DaemonPaths` (key included) from the recorded exe
path's current mtime and the current argv, env and `ferrex.toml`. A config edit or rebuild
during a round is then picked up on the next attempt instead of failing all three. The hello's
namespace comes from that same rebuilt config. `attempt` is an async closure that gets a fresh
`connect_or_spawn` stream and returns `Ok(Connected)` or an
`AttemptError` carrying the hello `kind` (if any) and whether the error is retryable.
`run_round` owns the counting, the backoff and "protocol/init end the round". `used` is the
number of attempts already spent in this round (0 except below), and `Connected` reports how
many the round used. On a loss while the kept id is "re-send pending", `reconnect` calls
`run_round` again with that count, so the three-attempt limit and the backoff carry across the
re-sends. T1's closure does
the hello. T3's closure does the hello and the `initialize` replay.

## Units

```
unit            | purpose (1 line)                                        | interface                                                       | depends on                   | files
daemon-addr     | key, run-dir paths, live-daemon lookup by db or any     | DaemonPaths::for_config(&FerrexConfig, exe: &Path) -> Result<Self>; PidFile read/write; live_for_db(run_dir, db_path) -> Option<u32>; any_live(run_dir) -> Option<u32> | ferrex-core config (Serialize) | crates/server/src/daemon/addr.rs; Serialize derives in crates/core/src/{types,staleness}.rs, crates/embed/src/lib.rs
handshake       | hello/reply types, encode/decode, version+key check     | Hello, HelloReply{ok,kind,error}, Hello::check(key)             | serde                        | crates/server/src/daemon/handshake.rs
spawn           | connect-or-spawn under flock                            | connect_or_spawn(&DaemonPaths, exe, argv) -> Result<UnixStream, AttemptError>; run_round(resolve, attempt, used) -> Result<Connected, RoundError> | daemon-addr                  | crates/server/src/daemon/spawn.rs
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
  proven by:  a LeaveRunning sidecar that spawned qdrant leaves it running, and its PID file in place, on drop and on shutdown; KillOnDrop keeps today's behaviour; killing qdrant under a LeaveRunning owner gets it reaped and restarted on the same port; killing qdrant after its owner has exited gets a LeaveRunning reuser to respawn it and take ownership, and two reusers produce one respawn; a `qdrant.pid` naming a live non-qdrant process (e.g. `sleep`) that doesn't answer the probe is treated as stale and a fresh qdrant starts; a live qdrant started through a wrapper script is reused; a `qdrant.port` that differs from the configured port fails at once with the port-mismatch error; a missing `qdrant.port` skips the check; a crashed qdrant left as a zombie under a `KillOnDrop` owner that is still running is treated as dead and respawned by a `LeaveRunning` reuser; `KillOnDrop` shutdown leaves a `qdrant.pid` naming another PID, and its `qdrant.port`, in place; `pid_alive` returns false for an unreaped zombie child (a plain unit test, not `#[ignore]`; it runs inside `nix build`'s check phase, which is why the check avoids a `ps` on `PATH`, and on macOS it skips with a message if `/bin/ps` is missing); `matches_qdrant_name` accepts `qdrant`, `/nix/store/.../qdrant` and `.qdrant-wrapped` (a pure test, no process lookup); `is_qdrant` on a PID whose name can't be read returns false rather than an error. A `start` whose `qdrant_bin` is a fake script that exits at once without listening (`#!/bin/sh` + `exit 1`, so `try_wait` fails the health check on its first poll instead of after the 30 s budget) fails the health check, leaves no live or zombie child, and removes `qdrant.pid` and `qdrant.port` (this one needs no real qdrant, so it isn't `#[ignore]`). The probe-fail re-check that finds a changed PID is a two-starter race, covered by review. A `qdrant.pid` under 30 s old naming a live non-qdrant process gets the full health check rather than one probe (a `sleep` whose PID file is backdated past 30 s still takes the single probe). A fake `qdrant_bin` that exits at once while a real qdrant already answers on the port makes `start` fail rather than succeed. All at the `QdrantSidecar` level, in `#[tokio::test(flavor = "multi_thread")]` since the watcher uses `Handle::block_on`, via its existing `base_dir: Option<PathBuf>` parameter with a temp dir and a free port. Today's `test_sidecar_lifecycle` passes `None` and port 6340; the new tests must not, so they never touch the developer's `~/.ferrex`. The `from_config_with` error path is a one-line policy pass-through, covered by review. These need a real qdrant binary, so they're `#[ignore]` like `test_sidecar_lifecycle` and run where qdrant is installed (the dev shell has it)
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

## Task Dependency Graph

```
T1 (AFK) daemon + proxy happy path, end to end
   units: daemon-addr (key, paths; not live_for_db), handshake, spawn (including attempt,
          backoff and failure counting), daemon (complete: hello, ensure_collection, idle exit,
          ownership-checked shutdown), proxy (startup, line framing, id tracking; no
          reconnect), cli, server (add with_session + SessionDefaults; the namespace is only
          logged at session start, which also keeps clippy quiet), Serialize prep refactor,
          sidecar-policy, path absolutising in build_config, the `daemon-paths` subcommand,
          PidFile helper, tests/common harness with the fake daemon, `is_qdrant` and `pid_alive` (`pid_alive` replacing `is_process_alive`; pub in
          ferrex-store and re-exported from its lib.rs with `SidecarPolicy`), `ferrex-store` promoted from dev-dependency to dependency in
          crates/server/Cargo.toml (the daemon holds the `QdrantSidecar` and `nuke` calls
          `is_qdrant`), and the `QdrantSidecar::start` call sites that gain a policy argument
          (crates/core/tests/common/mod.rs, crates/server/tests/mcp_protocol.rs pass KillOnDrop)
   deps:  none
   done:  `ferrex` with no args spawns the daemon and a second `ferrex` reuses it;
          tests/daemon_e2e.rs passes through the proxy; mcp_protocol.rs runs with --no-daemon
          and stays green; the PID file holds PID and db_path; the daemon idles out; a proxy
          session against the fake daemon (temp run dir, not-yet-existing --db-path dir, empty
          model cache) completes initialize + tools/list and leaves no SQLite file, no model
          files and no qdrant.pid behind; with three proxies on one real daemon, only the
          daemon holds the model files open (manual `lsof` check), and the daemon's and one
          proxy's RSS are recorded in the PR
T2 (AFK) per-session namespace defaults
   units: server (inject session namespace; hand-written get_info advertising tools.listChanged), core-namespace
   files: crates/server/src/server.rs, crates/core/src/service/{taxonomy,stats}.rs,
          crates/core/src/types.rs, the `diagnose` call in crates/server/src/commands.rs,
          crates/core/tests/diagnostics.rs and store_recall.rs (call sites of the changed
          signatures), tests/daemon_namespace.rs
   deps:  T1
   done:  tests/daemon_namespace.rs passes: two proxies with different FERREX_NAMESPACE on one
          daemon store/recall in their own namespaces and an explicit `namespace` param wins;
          `initialize` advertises tools.listChanged; crates/core diagnostics.rs and
          store_recall.rs stay green
T3 (AFK) reconnect and replay
   units: reconnect (in-flight errors, queue, initialize replay, post-initialize failure
          handling; reuses spawn's attempt loop)
   files: crates/server/src/proxy/reconnect.rs, a hook in crates/server/src/proxy.rs,
          tests/daemon_reconnect.rs
   deps:  T1
   done:  tests/daemon_reconnect.rs covers every `reconnect` proven-by item against the fake
          daemon, plus a real-daemon test: `kill -9` the daemon mid-session and the next
          request succeeds without the client re-initialising; T1 suites stay green
T4 (AFK) admin guard, nuke and docs
   units: daemon-addr (live_for_db, any_live), admin-guard, nuke stopping qdrant, README notes
          on the daemon, ferrexd.log and --no-daemon; design-doc notes that qdrant stays
          running in daemon mode (only KillOnDrop, i.e. --no-daemon and CLI commands, stops a
          qdrant it spawned; nuke stops it)
   files: crates/server/src/daemon/addr.rs (live_for_db and any_live, on top of T1's PidFile helper), crates/server/src/admin_guard.rs,
          guard calls in audit.rs, backfill.rs and nuke in commands.rs, README.md,
          docs/design/ferrex-rag-memory.md (sidecar "stops on shutdown"),
          docs/design/design-decisions.md (dated note on the "On ferrex exit → SIGTERM" step,
          pointing to this spec), tests/daemon_admin_guard.rs
   deps:  T1
   done:  tests/daemon_admin_guard.rs passes (reconcile --fix, backfill and nuke refuse with a
          live daemon on the same DB and succeed with none; a stale or unparsable PID file
          doesn't block); nuke stops qdrant per the admin-guard rules; the README and both
          design docs carry the notes above
```

T2, T3 and T4 each depend only on T1. They touch disjoint files except `commands.rs`, where T2
changes the `diagnose` call and T4 rewrites `nuke`. Those are separate functions,
so the edits merge cleanly, but run T2 and T4 one after the other if the executor needs strict
file disjointness.

## Agent Assignments

```
Agent assignments (auto-selected):
  Task 1: daemon + proxy happy path        → rust-engineer   (Rust)
  Task 2: per-session namespace defaults   → rust-engineer   (Rust)
  Task 3: reconnect and replay             → rust-engineer   (Rust)
  Task 4: admin guard, nuke and docs       → rust-engineer   (Rust + markdown)
  Polish:                                  → rust-engineer   (all-Rust diff)
```

## Calls I made (check these)

- The key covers the exe path + mtime + the whole resolved config minus namespace. The proxy
  computes it and the daemon re-checks it. Upgrades and config edits briefly run two daemons instead of
  refusing sessions.
- Idle timeout defaults to 10 minutes (`FERREX_IDLE_TIMEOUT`), armed once init finishes with no
  connections.
- Every tool follows the session's namespace. Collections are ensured at hello time, and
  `taxonomy` gets a small core change.
- The proxy frames lines and tracks ids from the start; it reconnects lazily on the next client
  line, replays `initialize`, and fails in-flight requests with a retryable error.
- Once the client is initialised the proxy never exits on daemon trouble; failures become
  JSON-RPC errors and the next request retries. Before that, startup failures exit as today.
- The daemon re-checks its key against its own config, so a config edit mid-spawn can't run
  the wrong settings.
- `--no-daemon` keeps the current in-process mode.
- The admin guard matches on database path across all daemon PID files, and it's best-effort.
- All daemons share one 0700 run dir (`~/.ferrex/run`, `FERREX_RUN_DIR` for tests) and one
  `ferrexd.log`.
- Daemons never stop the qdrant sidecar; it stays resident after the last session closes. The
  daemon that spawned it reaps it and restarts it (3 times per 10 min) if it crashes; reuser
  daemons poll it and respawn it if its owner is gone. A port mismatch with the running sidecar
  fails at once.
- `nuke` on `~/.ferrex` refuses while any daemon is alive, and stops qdrant itself after
  checking the PID is really qdrant. On a custom DB path it only refuses for daemons on that DB.
- Init times out after 30 minutes by default; a failed daemon keeps refusing with the real error
  for 60 s; shutdown ends with `process::exit` so a stuck model load can't keep it alive.
- Timing-sensitive proxy behaviour is tested against a fake daemon.

## Known risks carried over

- Startup recovery in a second `MemoryService` (an upgrade-overlap daemon, or a CLI command such
  as `diagnose`) can compensate ops a live daemon has in flight. That already happens today
  between sessions.

- A daemon killed with SIGKILL leaves its PID file behind. It reads as stale and is ignored.
- The run dir keeps one `<key>.lock` per key ever used, plus `.sock`/`.pid` after SIGKILL or a
  reboot. They're tiny and the liveness checks ignore them; the user may `rm -rf` the run dir
  when no daemon runs. No automatic `.lock` deletion: unlinking a flock file while another
  process opens it can leave two holders of the same lock.
- `nuke` stopping qdrant cuts off any `--no-daemon` session or CLI command using the sidecar at
  that moment, since the guard only sees daemons. Best-effort, as with the rest of the guard.
- On macOS, connecting to a Unix socket whose backlog is full fails with ECONNREFUSED. A proxy
  would then treat a live daemon's socket as stale, unlink it and spawn a second daemon. The
  daemon accepts continuously, even during init, so this needs an extreme burst of sessions.
- A daemon that is alive but wedged (not accepting, or never answering hellos) makes each proxy
  round fail after its timeouts, and nothing replaces it until it's killed. The PID file makes
  it easy to find, and `ferrexd.log` shows what it was doing.
- A qdrant that is alive but unresponsive fails the health check and errors, as today. No
  watcher restarts it, since it hasn't exited.
- An upgraded qdrant binary isn't picked up while the old sidecar runs: daemons keep reusing
  it. Changing `qdrant_port` likewise needs the old sidecar stopped, and that case fails at once
  with a message saying so. In both cases, live daemons on the old config respawn qdrant within
  ~2 s with their own binary and port, and `nuke` refuses while they live, so those daemons must
  exit first (close their sessions, or kill them), then stop qdrant. Sessions started before
  then get the init error for the 60 s grace period.
- Between qdrant dying and a watcher respawning it (up to 2 s of polling plus qdrant's
  startup), requests error and the Qdrant clients reconnect afterwards.
- `stats` with `diagnostics: true` reports no qdrant PID over MCP, in daemon mode as in today's
  stdio mode, because the service runs after `into_parts`; `ferrex diagnose` still shows it.
- A CLI command or `--no-daemon` session that spawned qdrant (because none was running) still
  stops it on exit, which would cut off a daemon that started in the meantime and reused it.
  The reuser's watcher respawns it within seconds, using one unit of its restart budget.
- The sidecar's own lock is a `create_new` file with a ~2 s acquire budget that only counts as
  stale after 30 s. If a process is killed, or exits through the daemon's signal path, inside
  `QdrantSidecar::start`'s lock section, new daemons fail
  init for up to 30 s. This exists today. Replacing that lock with a real flock (part of the "last user out" follow-up) fixes it.
- Any transient init failure (qdrant briefly down, the sidecar lock above) is held for the
  60 s init-failed grace period: every session that starts in that window fails at startup and
  needs `/mcp` reconnect, where today only the session that hit the blip would. That's the
  price of not repeating a ~1.5 GB model load per session for a deterministic failure.
- Shutdown removes the PID file before the sessions are cancelled. In that short window the
  admin guard can't see a daemon that may still write. This is covered by the guard being
  best-effort.
- One embedder and one reranker, each behind a `Mutex` (`crates/embed/src/lib.rs`), serve every
  session on a daemon, so a long rerank in one session delays embeds and recalls in the others.
  Today each session has its own models. The work is CPU-bound, so parallel copies would gain
  little and multiply memory, which defeats the goal.
- An orphan daemon (its spawner killed by a signal before the socket accepted) can bind between
  the next proxy's failed connect under the lock and its unlink. The proxy then deletes the
  orphan's live socket and spawns a second daemon; if the orphan wrote `<key>.pid` last, its
  idle exit removes that PID file and the live daemon is invisible to the admin guard. The
  window is microseconds wide and the guard is best-effort.

## Follow-ups (not in this work)

- Rerank in fixed-size batches (measured: 40 docs went from 4.37 GB to 2.16 GB with batch 4).
- Load the reranker on first recall.
- Stop the qdrant sidecar once nothing uses it ("last user out"). That needs every qdrant user
  to register, plus a real flock in place of the sidecar's `create_new` lock.
- qdrant-client prints a version-check warning to stdout at startup. In standalone mode this
  corrupts the MCP stream. The sidecar health-check and probe clients get
  `skip_compatibility_check()` in this work; `VectorStore::new` (`crates/store/src/vector.rs`)
  still builds its client without it and should get it too.
