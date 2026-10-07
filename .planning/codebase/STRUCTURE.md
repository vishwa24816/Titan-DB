# Codebase Structure

**Analysis Date:** 2026-10-07

## Directory Layout

```
Titan-DB/
├── src/                # Library crate + binaries
│   ├── lib.rs          # Crate root, module declarations
│   ├── main.rs         # CLI demo binary (default `cargo run`)
│   ├── error.rs        # TitanError + Result alias
│   ├── storage/        # Pager, Page definitions
│   ├── index/          # B-Link Tree implementation
│   ├── sql/            # Executor and result types
│   ├── catalog/        # Schema management
│   └── bin/            # Additional binaries (server, tests, tools)
├── web/                # Frontend assets (admin UI)
├── bin/                # Checked-in Windows binary artifact
├── Cargo.toml          # Package manifest + dependencies
├── Cargo.lock          # Pinned dependency tree
├── README.md           # Setup, SQL examples, structure overview
├── p.md                # Repo notes (check before planning)
└── .planning/          # GSD planning docs (this map lives here)
```

## Directory Purposes

**`src/`:**
- Purpose: Entire Rust crate — library modules plus all binaries.
- Contains: `*.rs` files, `bin/` subdirectory.
- Key files: `src/lib.rs`, `src/main.rs`, `src/error.rs`

**`src/storage/`:**
- Purpose: Durability layer — page model and file-backed buffer pool.
- Contains: Pager (sharded cache + file I/O), page/MVCC record structs.
- Key files: `src/storage/pager.rs`, `src/storage/page.rs`, `src/storage/mod.rs`

**`src/index/`:**
- Purpose: Access-path layer — B-Link tree over the pager.
- Contains: Tree navigation, MVCC-filtered search/insert/delete/scan.
- Key files: `src/index/blink.rs`, `src/index/mod.rs`

**`src/sql/`:**
- Purpose: Language layer — SQL parsing, execution, result shaping.
- Contains: Statement dispatcher, `ExecutionResult` enum.
- Key files: `src/sql/executor.rs`, `src/sql/mod.rs`

**`src/catalog/`:**
- Purpose: Schema registry — table definitions and index handles.
- Contains: `Catalog`, `TableSchema`, `ColumnDef`, `DataType`.
- Key files: `src/catalog/mod.rs`

**`src/bin/`:**
- Purpose: Secondary binaries built with `cargo run --bin <name>`.
- Contains: Async server, concurrency tests, dataset seeder.
- Key files: `src/bin/server.rs`, `src/bin/concurrency_test.rs`, `src/bin/dataset_populator.rs`, `src/bin/ws_concurrency_test.rs`

**`web/`:**
- Purpose: Built-in admin UI served verbatim by warp.
- Contains: Static HTML/JS; no build step.
- Key files: `web/index.html`

**`bin/`:**
- Purpose: Prebuilt Windows executable artifact (`titan_db.exe`).
- Contains: Compiled binary, not source. Do not edit; rebuild via cargo.

## Key File Locations

**Entry Points:**
- `src/main.rs`: CLI demo — wipes `titan_sql.db`, runs CREATE/INSERT/SELECT/ALTER/DROP showcase.
- `src/bin/server.rs`: Production entry — tokio+warp on `127.0.0.1:3030`, static `web/` + `/ws` SQL endpoint.
- `src/bin/concurrency_test.rs`: Correctness/stress harness — 150-thread MVCC + persistence assertions.
- `src/bin/dataset_populator.rs`: WS seeder client — creates 20-col `dataset` table, inserts 200 rows.
- `src/bin/ws_concurrency_test.rs`: WS load harness against a live server.
- `src/lib.rs`: Library root for downstream `use titan_db::...` imports.

**Configuration:**
- `Cargo.toml`: Crate name `titan_db` v0.1.0, edition 2021, all dependencies (tokio, warp, sqlparser, serde, bincode, parking_lot).
- `Cargo.lock`: Pinned versions — commit it, don't hand-edit.
- `.gitignore`: Excludes `target/`, `*.db` files, IDE dirs (verify before adding generated artifacts).

**Core Logic:**
- `src/sql/executor.rs`: Statement dispatch — the file most features touch first.
- `src/index/blink.rs`: Index logic — `find_leaf`, `search`, `insert`, `delete`, `scan_all`.
- `src/storage/pager.rs`: Buffer pool + file I/O — `open`, `fetch_page`, `allocate_page`, `flush_page`.
- `src/storage/page.rs`: On-disk record format — `PageHeader`, `MvccRecord`, `PAGE_SIZE`.
- `src/catalog/mod.rs`: Schema types — `Catalog`, `TableSchema`.

**Testing:**
- `src/bin/concurrency_test.rs`: Primary verification harness (run with `cargo run --bin concurrency_test`).
- `src/bin/ws_concurrency_test.rs`: Server-side load verification.
- No `tests/` dir, no `*.test.rs` / `*_test.rs` unit tests, no framework config detected.

## Naming Conventions

**Files:**
- `snake_case.rs` throughout (`pager.rs`, `blink.rs`, `executor.rs`, `server.rs`, `dataset_populator.rs`, `concurrency_test.rs`, `ws_concurrency_test.rs`).
- `mod.rs` as module root in every subdirectory (`storage/mod.rs`, `index/mod.rs`, `sql/mod.rs`, `catalog/mod.rs`).
- Binaries named by role: `server`, `concurrency_test`, `dataset_populator`, `ws_concurrency_test`.

**Directories:**
- Lowercase singular nouns matching the architectural layer: `storage/`, `index/`, `sql/`, `catalog/`, `bin/`, `web/`.
- `src/bin/` follows the Cargo auto-binary convention — every `*.rs` file is a separate binary target.

**Symbols (Rust defaults, observed):**
- Types `UpperCamelCase` (`BLinkTree`, `Pager`, `TableSchema`, `ExecutionResult`, `TitanError`).
- Functions/methods/fields `snake_case` (`fetch_page`, `find_leaf`, `tx_created`, `root_page_id`).
- Constants `SCREAMING_SNAKE_CASE` (`PAGE_SIZE`, `SHARD_COUNT`, `GLOBAL_TX_ID`).

## Where to Add New Code

**New SQL statement support (e.g. JOIN, GROUP BY, CREATE INDEX):**
- Primary code: `src/sql/executor.rs` — add a `Statement::` match arm in `execute_statement` or extend `execute_query`.
- Schema changes: `src/catalog/mod.rs` if new catalog state is needed.
- Tests: extend `src/main.rs` demo flow or add cases to `src/bin/concurrency_test.rs` (no unit-test harness exists yet).

**New index structure or tree operation (e.g. split propagation, range scan):**
- Implementation: `src/index/blink.rs` — extend `BLinkTree` methods; keep `Pager`-only dependency.
- Page format changes: `src/storage/page.rs` — update `PageHeader`/`NodeContent` plus `serialize`/`deserialize` together.
- Re-export: `src/index/mod.rs` if adding a new submodule file.

**New storage capability (e.g. WAL, LRU eviction, compression):**
- Implementation: `src/storage/pager.rs` and/or `src/storage/page.rs`.
- Error variants: `src/error.rs` — add `TitanError` variant, re-export is automatic via `src/lib.rs`.
- Wiring: `src/storage/mod.rs` if adding a new file module.

**New binary / tool:**
- Implementation: `src/bin/<snake_case_name>.rs` — Cargo picks it up automatically as `--bin <snake_case_name>`.
- Shared logic stays in `src/` library modules; bins only compose `Pager` + `Catalog` + `Executor` as `src/bin/server.rs` does.

**New UI / endpoint:**
- Static UI: `web/index.html` (served verbatim — no build step).
- Server route: `src/bin/server.rs` — add `warp` filters alongside `static_files` / `ws_route`; serialize replies via `ExecutionResult` in `src/sql/mod.rs`.

**Utilities:**
- Shared helpers: inline in the owning layer module (no `utils/` or `common/` dir exists — do not create one without need).
- Cross-cutting error handling: `src/error.rs`.

## Special Directories

**`.planning/`:**
- Purpose: GSD-generated planning and codebase-map docs.
- Generated: Yes (by `/gsd-map-codebase` and planning commands).
- Committed: Yes — keep in sync, never store secrets here.

**`bin/` (repo root):**
- Purpose: Holds `titan_db.exe` build artifact.
- Generated: Yes (compiler output checked in).
- Committed: Yes (unusual — prefer `cargo build --release` output in `target/` for future artifacts).

**`target/` (when built):**
- Purpose: Cargo build cache and binary output.
- Generated: Yes.
- Committed: No (gitignored).

---

*Structure analysis: 2026-10-07*
