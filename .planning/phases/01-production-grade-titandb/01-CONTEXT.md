# Phase 1: Production-Grade TitanDB - Context

**Gathered:** 2026-10-07
**Status:** Ready for planning

## Phase Boundary

Take the existing Titan-DB prototype (single-leaf B-link stub, in-memory catalog, per-statement tx ids, SELECT-ignores-WHERE, sync-flush pager) to a production-level SQLite-alternative: real B+tree concurrency, MVCC transactions, full SQL function surface, WAL durability, and single-exe pgAdmin-style frontend. Verdict: vision NOT achieved — this phase closes the gap.

## Implementation Decisions

### Correctness / Storage Engine
- **D-01:** Full B+tree with Lehman-Yao splits (and merges) — high_key/right_link invariants maintained under concurrency, with split tests written first (`src/index/blink.rs:26-71` is the expansion point).
- **D-02:** Fix `scan_all()` visibility to newest-first per-key (match `search()` semantics in `src/index/blink.rs:79`); fix oversize-page truncation to error + split/overflow instead of corrupting neighbors.
- **D-03:** Replace `std::sync::Mutex::unwrap` with `parking_lot` + `TitanError::LockError`; fix `fetch_page` TOCTOU and fresh-DB `UnexpectedEof`.

### Transactions / MVCC
- **D-04:** Full transactions: BEGIN/COMMIT/ROLLBACK, snapshot isolation, read-timestamp pinning, conflict detection, background vacuum/GC of expired versions (replaces `GLOBAL_TX_ID`-per-statement in `src/sql/executor.rs:14`).
- **D-05:** Transaction-capable WebSocket: dedicated fast transaction channel (session-pinned tx context over WS) in addition to single-statement WS — not just a SQL API, a WS transaction protocol.
- **D-06:** WAL + LSN + `sync_all` on commit + checkpoint + crash recovery replay (LSN field `src/storage/page.rs:27` currently dead); catalog persisted (reserved catalog page or sidecar) and rebuilt on open — restarts must not lose tables.

### SQL Surface
- **D-07:** Full function set: numeric, string, date/time, conditional logic (CASE/COALESCE/NULLIF), operators, aggregates (COUNT/SUM/AVG/MIN/MAX), window functions (ROW_NUMBER/RANK/OVER/PARTITION BY).
- **D-08:** Indexing + JOINs (inner/left/right, predicate pushdown — route `WHERE id = ?` to `BLinkTree::search()` instead of full `scan_all`), WHERE/ORDER BY/LIMIT evaluation from sqlparser AST, honest errors for unsupported syntax (no silent 0-row success, no fake ALTER/DROP success).
- **D-09:** Replace `|||`-delimited row encoding with length-prefixed typed tuple encoding (type fidelity, NULLs, escaping) decoded by column type.

### Product / Performance
- **D-10:** Single-exe packaging running pgAdmin-style frontend (`web/index.html`) with query executor + transaction manager; DB path via CLI arg/env (not CWD-relative `titan_web.db`); token auth + WS message caps/rate limits before any non-local exposure.
- **D-11:** Multicore scaling: positional I/O (pread/pwrite) or per-thread handles to drop the global `File` mutex, cache-hit reads without file lock, group-commit/background flusher; page size revisited (8/16 KiB vs current 64 KiB) with benchmarks.
- **D-12:** Success = `cargo test` suite (pager crash-consistency, MVCC matrix, SQL semantics, concurrency) replacing eyeball-only binaries; existing `src/bin/concurrency_test.rs` assertions (150 ops, snapshot isolation, persistence) become automated tests.

### the agent's Discretion
- Exact WAL record format, page-size final value, WS transaction wire protocol details, exe bundling mechanism (embed web/ vs sidecar) — researcher/planner decide, guided by benchmarks.

## Canonical References

**Downstream agents MUST read these before planning or implementing.**

### Codebase maps (repo-local analysis)
- `.planning/codebase/CONCERNS.md` — Full gap/bug inventory (single-leaf, catalog loss, no WAL, wrong scan_all, 64 KiB flush-per-op); the gap list for this phase
- `.planning/codebase/ARCHITECTURE.md` — Layer/pattern/entry-point map
- `.planning/codebase/STRUCTURE.md` — Directory layout, key file locations
- `.planning/codebase/STACK.md` — Deps (sqlparser 0.43, warp 0.3, bincode 1.3, tokio, parking_lot)
- `.planning/codebase/TESTING.md` — Zero-test baseline, binary-only verification

### In-code specs (load-bearing comments)
- `src/index/blink.rs:50-70` — Interior-navigation TODO (what to implement)
- `src/sql/executor.rs:185-217` — Query path that ignores WHERE/JOIN (what to replace)
- `src/storage/pager.rs:74-109` — allocate/flush path needing WAL + sync_all

No external specs — requirements fully captured in decisions above.

## Existing Code Insights

### Reusable Assets
- `src/storage/pager.rs` (sharded buffer pool, 16 shards): keep sharding, fix lock order + TOCTOU + positional I/O
- `src/index/blink.rs` (B-link skeleton, MVCC `search/insert/delete`): keep record shape, add descent/split/merge + correct scan
- `src/sql/executor.rs` (sqlparser PostgreSqlDialect front-end): keep parser, rewrite lowering to be structural not stringly-typed
- `src/catalog/mod.rs` (schema structs): keep shapes, add persistence
- `src/bin/server.rs` + `web/index.html`: keep WS+UI skeleton, add tx protocol + auth + limits
- `src/bin/concurrency_test.rs`, `src/bin/ws_concurrency_test.rs`: convert assertions into `cargo test`s

### Established Patterns
- `TitanError` + `Result<T>` (`src/error.rs`): extend with new variants (oversize page, corrupt link, unsupported SQL, tx conflict) instead of `expect`/`unwrap`
- `parking_lot::RwLock` for pages/catalog: standardize on it everywhere (drop `std::sync::Mutex`)

### Integration Points
- Pager ↔ BLinkTree (page alloc/fetch/flush) — WAL hooks here
- Executor ↔ Catalog ↔ Tree (statement → schema → storage) — tx context threads through here
- Server WS ↔ Executor — tx-session pinning here

## Specific Ideas

- User's words: "alternative sqlite with MVCC, ultrafast low-latency sharding, independent ops proceed without blocking anything; exe with pgAdmin-like frontend, query executor + transaction manager, multicore scaling, all normal-SQL features; B+tree for concurrency/stability; commits, transactions, window/numeric/string/aggregate/conditional/operator functions; transaction websocket for faster transactions."
- No visual mockups — standard pgAdmin-style result grid is fine.

## Deferred Ideas

None — discussion stayed within phase scope.

---

*Phase: 1-Production-Grade TitanDB*
*Context gathered: 2026-10-07*
