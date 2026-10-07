<!-- refreshed: 2026-10-07 -->
# Architecture

**Analysis Date:** 2026-10-07

## System Overview

```text
┌─────────────────────────────────────────────────────────────┐
│                    Interface Layer                           │
├──────────────────┬──────────────────┬───────────────────────┤
│   CLI Demo       │  Async Server    │  Test / Util Bins     │
│  `src/main.rs`   │ `src/bin/server.rs` │ `src/bin/concurrency_test.rs` │
│                  │                  │ `src/bin/dataset_populator.rs`  │
│                  │  `web/index.html`│ `src/bin/ws_concurrency_test.rs`  │
└────────┬─────────┴────────┬─────────┴──────────┬────────────┘
         │                  │                     │
         ▼                  ▼                     ▼
┌─────────────────────────────────────────────────────────────┐
│                    SQL Layer                                 │
│         `src/sql/executor.rs`, `src/sql/mod.rs`              │
│         `src/catalog/mod.rs`                                 │
└─────────────────────────────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────────────────────┐
│                    Index Layer (B-Link Tree)                 │
│         `src/index/blink.rs`, `src/index/mod.rs`             │
└─────────────────────────────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────────────────────┐
│                    Storage Layer (Sharded Pager)             │
│         `src/storage/pager.rs`, `src/storage/page.rs`        │
│         On-disk file: `titan_sql.db` / `titan_web.db`        │
└─────────────────────────────────────────────────────────────┘
```

## Component Responsibilities

| Component | Responsibility | File |
|-----------|----------------|------|
| Executor | Parses PostgreSQL-dialect SQL via `sqlparser`, dispatches DDL/DML/DQL to catalog + index | `src/sql/executor.rs` |
| ExecutionResult | Wire/display type for query output (`Message` vs `ResultSet`); serialized to JSON over WS | `src/sql/mod.rs` |
| Catalog | In-memory `HashMap<table_name, TableSchema>`; owns per-table `Arc<BLinkTree>` + root page id | `src/catalog/mod.rs` |
| BLinkTree | Key-value index with MVCC visibility filtering, move-right traversal, per-key version chains | `src/index/blink.rs` |
| Pager | Sharded (16-way) buffer pool, page fetch/allocate/flush, file-backed persistence | `src/storage/pager.rs` |
| Page / MvccRecord | 64KB page model: `PageHeader` (high_key, right_link, LSN) + `NodeContent` (MVCC records) | `src/storage/page.rs` |
| Error | Unified `TitanError` enum (`thiserror`) + `Result<T>` alias | `src/error.rs` |
| Crate root | Re-exports modules and error types for bins | `src/lib.rs` |
| Async server | `tokio` + `warp` static file host + `/ws` WebSocket SQL endpoint | `src/bin/server.rs` |
| CLI demo | Synchronous end-to-end demo: CREATE/INSERT/SELECT/ALTER/DROP | `src/main.rs` |
| Web UI | Single-page PGAdmin-style admin console (SQL over WebSocket) | `web/index.html` |

## Pattern Overview

**Overall:** Embedded library database with layered synchronous core + thin async server adapter.

**Key Characteristics:**
- Library-first: storage + index + SQL are `Sync`-oriented library code exposed via `src/lib.rs`; binaries are consumers.
- Shared-ownership concurrency: `Arc<Pager>` + `Arc<RwLock<Catalog>>` + `Arc<BLinkTree>` cloned into threads/tasks.
- No global lock on reads: 16-shard buffer pool keyed by `page_id % 16` plus per-page `RwLock`.
- MVCC via version chains: every `MvccRecord` carries `tx_created` / `tx_expired`; readers filter by `tx_read_ts`.

## Layers

**Interface layer:**
- Purpose: Entry points for humans, browsers, and load tests.
- Location: `src/main.rs`, `src/bin/*.rs`, `web/index.html`
- Contains: CLI demo flow, warp HTTP+WS server, WS message loop, concurrency/dataset harnesses.
- Depends on: SQL layer (`Executor`, `ExecutionResult`, `Catalog`, `Pager`).
- Used by: External callers (browser, `tokio-tungstenite` client, operator running `cargo run`).

**SQL + Catalog layer:**
- Purpose: SQL parsing, statement dispatch, schema registry.
- Location: `src/sql/executor.rs`, `src/sql/mod.rs`, `src/catalog/mod.rs`
- Contains: `Executor::execute` / `execute_statement` / `execute_query`, `TableSchema`, `ColumnDef`, `DataType`, `GLOBAL_TX_ID`.
- Depends on: Index layer (`BLinkTree`) and storage layer (`Pager`).
- Used by: All binaries.

**Index layer:**
- Purpose: Ordered key access with concurrency-friendly navigation and snapshot reads.
- Location: `src/index/blink.rs`
- Contains: `BLinkTree { pager, root }`, `find_leaf`, `search`, `insert`, `delete`, `scan_all`.
- Depends on: Storage layer only.
- Used by: SQL layer (one `BLinkTree` per table, held inside `TableSchema`).

**Storage layer:**
- Purpose: Page lifecycle and durability.
- Location: `src/storage/pager.rs`, `src/storage/page.rs`
- Contains: `Pager { file: Mutex<File>, shards: Vec<RwLock<Shard>>, total_pages }`, `Page::serialize/deserialize`, `MvccRecord`.
- Depends on: `src/error.rs` only.
- Used by: Index layer and (transitively) everything above.

## Data Flow

### Primary Request Path

1. SQL text arrives — CLI literal (`src/main.rs:28`), WS text frame (`src/bin/server.rs:48`), or test harness string (`src/bin/concurrency_test.rs:44`).
2. Parse with `PostgreSqlDialect` — `Parser::parse_sql` in `src/sql/executor.rs:32`.
3. Dispatch in `execute_statement` (`src/sql/executor.rs:42`): `CreateTable` allocates a `BLinkTree` + registers `TableSchema` (`src/sql/executor.rs:44`); `Insert`/`Update`/`Delete` mint `tx_id` via `GLOBAL_TX_ID` (`src/sql/executor.rs:27`) and call `tree.insert/delete`; `Query` calls `execute_query` (`src/sql/executor.rs:185`).
4. Index resolves leaf via `find_leaf` (`src/index/blink.rs:28`) with move-right on `high_key` (`src/index/blink.rs:36`), then reads/writes `MvccRecord`s with `tx_created`/`tx_expired` filtering (`src/index/blink.rs:79`, `src/index/blink.rs:143`).
5. Pager fetches from shard cache or disk (`src/storage/pager.rs:52`), mutates in-memory page, marks `dirty`, and `flush_page` serializes with `bincode` + zero-pads to `PAGE_SIZE` and writes at `page_id * PAGE_SIZE` (`src/storage/pager.rs:90`).
6. Result returns as `ExecutionResult` (`src/sql/mod.rs:6`) — `Display` for CLI, `serde_json` for WS (`src/bin/server.rs:53`).

### WebSocket Serve Flow

1. `warp::serve` binds `127.0.0.1:3030` with `warp::fs::dir("web")` + `/ws` route (`src/bin/server.rs:20`, `src/bin/server.rs:24`).
2. `handle_ws` loops on incoming text frames, calls `executor.execute(text)`, sends back JSON-encoded `ExecutionResult` (`src/bin/server.rs:36`).
3. Browser UI in `web/index.html` opens `ws://.../ws`, sends SQL, renders JSON result set.

### Concurrency / MVCC Flow

1. `GLOBAL_TX_ID: AtomicU64` (`src/sql/executor.rs:14`) mints monotonically increasing write timestamps and read snapshots (`next_tx_id`, `src/sql/executor.rs:26`).
2. Writers append a new `MvccRecord` and expire the previous live version (`src/index/blink.rs:94`); deletes only set `tx_expired` (`src/index/blink.rs:120`).
3. Readers pass `tx_read_ts` and accept only `tx_created <= ts < tx_expired` (`src/index/blink.rs:81`, `src/index/blink.rs:151`); old snapshot in `src/bin/concurrency_test.rs:91` proves isolation.

**State Management:**
- Durable state: heap file (`titan_sql.db`, `titan_web.db`) of fixed 64KB pages; `total_pages` derived from file length on open (`src/storage/pager.rs:31`).
- In-memory state: sharded `HashMap<PageId, Arc<RwLock<Page>>>` buffer pool; `Catalog.tables` map; per-tree `root: Mutex<PageId>`.
- No WAL/checkpoint: durability is write-through `flush_page` per mutation.

## Key Abstractions

**Pager (sharded buffer pool):**
- Purpose: Contention-free page cache + file manager.
- Examples: `src/storage/pager.rs`
- Pattern: `page_id % 16` shard routing; double-checked fetch (read-lock → disk read → write-lock insert); `Mutex<File>` for positional I/O; `Mutex<PageId>` allocator.

**Page (B-Link MVCC node):**
- Purpose: Single serializable unit combining B-Link navigation metadata with versioned tuples.
- Examples: `src/storage/page.rs`
- Pattern: `PageHeader { page_id, page_type, lsn, high_key, right_link }` + `NodeContent { records: Vec<MvccRecord> }`; `bincode` serde with zero-pad to `PAGE_SIZE = 65536`.

**BLinkTree (per-table index):**
- Purpose: The only access path for a table's rows.
- Examples: `src/index/blink.rs`, owned by `src/catalog/mod.rs:21`
- Pattern: Root-anchored tree; `find_leaf` move-right-then-descend; key = first-column bytes, value = `|||`-joined row string.

**Executor (SQL facade):**
- Purpose: Single `execute(&str) -> Result<ExecutionResult>` entry for all SQL.
- Examples: `src/sql/executor.rs:30`
- Pattern: Parse-then-match on `sqlparser::ast::Statement`; catalog read/write locks scoped per statement type.

**Catalog (schema registry):**
- Purpose: Table-name → schema + index handle mapping.
- Examples: `src/catalog/mod.rs:28`
- Pattern: Plain `HashMap`, guarded externally by `parking_lot::RwLock`; `TableSchema` bundles `columns`, `root_page_id`, and live `Arc<BLinkTree>`.

## Entry Points

**CLI demo (`cargo run`):**
- Location: `src/main.rs`
- Triggers: Manual invocation; deletes `titan_sql.db`, replays CREATE/INSERT/SELECT/ALTER/DROP.
- Responsibilities: Smoke-test of the engine without networking.

**Async server (`cargo run --bin server`):**
- Location: `src/bin/server.rs`
- Triggers: Operator starts service; browser hits `http://localhost:3030`, WS clients hit `/ws`.
- Responsibilities: Owns the single shared `Arc<Executor>`; multiplexes concurrent SQL over warp tasks.

**Concurrency test (`cargo run --bin concurrency_test`):**
- Location: `src/bin/concurrency_test.rs`
- Triggers: Manual perf/correctness run.
- Responsibilities: 150-thread insert/update/delete mix, MVCC snapshot assertion, disk-reopen persistence check.

**Dataset populator (`cargo run --bin dataset_populator`):**
- Location: `src/bin/dataset_populator.rs`
- Triggers: Manual; requires running server.
- Responsibilities: WS client that creates 20-column `dataset` table and streams 200 random rows.

**WS concurrency test:**
- Location: `src/bin/ws_concurrency_test.rs`
- Triggers: Manual load test against live server.
- Responsibilities: Parallel WebSocket SQL pressure (verify file before modifying).

**Library (`titan_db` crate):**
- Location: `src/lib.rs`
- Triggers: `use titan_db::storage::pager::Pager` etc. from any bin or downstream crate.
- Responsibilities: Module declarations + `TitanError`/`Result` re-export.

## Architectural Constraints

- **Threading:** Synchronous core (`parking_lot::RwLock`, `std::Mutex`) shared across OS threads (`src/bin/concurrency_test.rs:43`) and across `tokio` WS tasks via `Arc<Executor>` (`src/bin/server.rs:15`). Blocking file I/O under `Mutex<File>` runs inside async handlers — do not add long critical sections there.
- **Global state:** `static GLOBAL_TX_ID: AtomicU64` in `src/sql/executor.rs:14` is the sole process-wide timestamp oracle; never reset it mid-run or MVCC visibility breaks.
- **Circular imports:** None — dependency direction is strictly interface → sql/catalog → index → storage → error.
- **Single-page tables:** `scan_all` reads only the root page (`src/index/blink.rs:143`); interior descent in `find_leaf` is stubbed (`src/index/blink.rs:68`). Keep all table data fittable on the root leaf until split propagation is implemented.
- **Encoding contract:** Row values are `|||`-joined column strings (`src/sql/executor.rs:94`); keys are raw first-column bytes (`src/sql/executor.rs:85`). Changing either breaks `execute_query` decoding (`src/sql/executor.rs:207`).

## Anti-Patterns

### Stubbed DDL paths that silently succeed

**What happens:** `ALTER TABLE` and `DROP` return success messages without touching the catalog or pager (`src/sql/executor.rs:175`, `src/sql/executor.rs:178`).
**Why it's wrong:** Callers (CLI demo, UI) believe schema changed when nothing persisted.
**Do this instead:** Follow the `CreateTable` pattern in `src/sql/executor.rs:44` — mutate `catalog.tables` and allocate/invalidate pages, or return an explicit "not supported" error.

### Blocking mutex inside async WS handler

**What happens:** `handle_ws` calls synchronous `executor.execute` → `Pager::flush_page` takes `std::Mutex<File>` while running on a tokio worker (`src/bin/server.rs:36`, `src/storage/pager.rs:102`).
**Why it's wrong:** Under concurrent WS load this blocks the async runtime thread.
**Do this instead:** Offload execution with `tokio::task::spawn_blocking` around `executor.execute`, keeping the pattern used for thread spawning in `src/bin/concurrency_test.rs:43`.

## Error Handling

**Strategy:** `thiserror`-derived `TitanError` (`Io`, `PageNotFound`, `Serialization`, `LockError`) propagated with `?`; SQL-layer maps domain errors (missing table, duplicate table) to `TitanError::Io` with embedded messages.

**Patterns:**
- Pager lock poisoning collapsed to `TitanError::LockError` (`src/storage/pager.rs:59`).
- Parser failures wrapped as `TitanError::Io` (`src/sql/executor.rs:33`).
- WS layer never propagates — errors are serialized into `ExecutionResult::Message("Error: ...")` (`src/bin/server.rs:54`).

## Cross-Cutting Concerns

**Logging:** `println!`/`eprintln!` only (`src/bin/server.rs:49`, `src/main.rs:21`). No structured logging facade.
**Validation:** Minimal — duplicate-table and missing-table checks in `src/sql/executor.rs:48` and `src/sql/executor.rs:74`; no type/constraint enforcement beyond INT-vs-Text column mapping (`src/sql/executor.rs:59`).
**Authentication:** None — server binds unauthenticated localhost WS + static files (`src/bin/server.rs:33`).

---

*Architecture analysis: 2026-10-07*
