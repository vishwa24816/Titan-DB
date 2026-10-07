# Coding Conventions

**Analysis Date:** 2026-10-07

## Naming Patterns

**Files:**
- `snake_case.rs` for all modules; one primary type per file (e.g., `src/storage/pager.rs` → `Pager`, `src/storage/page.rs` → `Page`/`PageHeader`, `src/index/blink.rs` → `BLinkTree`, `src/sql/executor.rs` → `Executor`, `src/error.rs` → `TitanError`)
- Module dirs use `mod.rs` re-export style: `src/storage/mod.rs` (`pub mod page; pub mod pager;`), `src/catalog/mod.rs`, `src/sql/mod.rs`, `src/index/mod.rs`
- Binaries in `src/bin/` are `snake_case` task names: `src/bin/server.rs`, `src/bin/concurrency_test.rs`, `src/bin/ws_concurrency_test.rs`, `src/bin/dataset_populator.rs`

**Functions:**
- `snake_case` verbs: `Pager::open`, `Pager::fetch_page`, `Pager::allocate_page`, `Pager::flush_page`, `BLinkTree::new/search/insert/delete/scan_all/find_leaf/root_page_id`, `Page::new/serialize/deserialize`, `Executor::new/execute/execute_statement/execute_query/next_tx_id`
- Private helpers are `snake_case` with `fn` + `get_`/`find_`/`execute_` prefix: `Pager::get_shard`, `BLinkTree::find_leaf`

**Variables:**
- `snake_case` throughout: `page_id`, `tx_id`, `tx_read_ts`, `table_name`, `leaf_id`, `current_id`
- Type aliases use `PascalCase`: `PageId` (`src/storage/page.rs`), `Result<T>` alias in `src/error.rs`
- Constants `SCREAMING_SNAKE_CASE`: `PAGE_SIZE` (`src/storage/page.rs`), `SHARD_COUNT` (`src/storage/pager.rs`); statics too: `GLOBAL_TX_ID` (`src/sql/executor.rs`)

**Types:**
- `PascalCase` structs/enums: `Pager`, `Page`, `PageHeader`, `PageType`, `MvccRecord`, `BLinkTree`, `Executor`, `Catalog`, `TableSchema`, `ColumnDef`, `DataType`, `TitanError`, `ExecutionResult`
- Enum variants `PascalCase`: `TitanError::Io/PageNotFound/Serialization/LockError`, `DataType::Integer/Text`, `PageType` variants in `src/storage/page.rs`

## Code Style

**Formatting:**
- Standard `rustfmt` style (4-space indent, trailing commas, `use` block at top). No `rustfmt.toml` / `.rustfmt.toml` present — use `cargo fmt` defaults.
- Run before every commit: `cargo fmt --check` / `cargo fmt`

**Linting:**
- No `clippy.toml`, no ESLint/Prettier (Rust-only backend plus `web/` statics). Use `cargo clippy -- -D warnings`.
- No CI config (no `.github/`) to enforce it — run locally.

## Import Organization

**Order:**
1. `std` imports first (`std::sync::Arc`, `std::collections::HashMap`, `std::fs::{File, OpenOptions}`)
2. External crates (`parking_lot::RwLock`, `sqlparser::ast::...`, `tokio`, `warp`)
3. `crate::` imports last (`crate::error::{Result, TitanError}`, `crate::storage::pager::Pager`)

**Path Aliases:**
- No `[lib] paths` / alias remapping in `Cargo.toml`. Crate name is `titan_db`; bins import as `use titan_db::storage::pager::Pager;` (see `src/main.rs`, `src/bin/server.rs`, `src/bin/concurrency_test.rs`). Inside the lib use full `crate::` paths (e.g., `crate::storage::page::PageType` in `src/storage/pager.rs`).

## Error Handling

**Patterns:**
- Single error enum with `thiserror` in `src/error.rs`: `#[derive(Error, Debug)] pub enum TitanError` with `#[from]` for `std::io::Error` and `bincode::Error`, plus domain variants `PageNotFound(u64)` and `LockError`. Shared alias `pub type Result<T> = std::result::Result<T, TitanError>;` — use it as the return type for every fallible lib function (`Pager::open/fetch_page`, `BLinkTree::search/insert`, `Executor::execute`).
- Propagate with `?` everywhere in library code (`src/storage/pager.rs`, `src/sql/executor.rs`). Convert foreign errors at the boundary with `map_err`: sql-parse failures in `src/sql/executor.rs` (`Parser::parse_sql(...).map_err(|e| TitanError::Io(...))`), poisoned `std::sync::Mutex` in `src/storage/pager.rs` (`.lock().map_err(|_| TitanError::LockError)?`).
- Ad-hoc domain errors are constructed as `TitanError::Io(std::io::Error::new(ErrorKind::Other, "..."))` (e.g., "Table already exists" in `src/sql/executor.rs`) — follow this until a richer variant is added to `src/error.rs`.
- `main`/bins (`src/main.rs`, `src/bin/server.rs`) use `Result<(), Box<dyn std::error::Error>>` or `.expect("...")` at the top level only. Never use `.unwrap()`/`.expect()` in library code except for lock access on `parking_lot` types (which don't poison) — e.g., `self.root.lock().unwrap()` in `src/index/blink.rs` is idiomatic because `parking_lot::Mutex::lock` returns the guard directly.
- Match on `Result` at user-facing boundaries: `src/bin/server.rs` serializes `Ok(res)` vs `Err(e)` into `ExecutionResult::Message(format!("Error: {}", e))` instead of crashing the server.

## Logging

**Framework:** `println!` only — no `log`, `tracing`, or `env_logger` in `Cargo.toml` or `src/`.

**Patterns:**
- Demo/progress output via `println!("SQL> {}", ...)`, `println!("RES: {}", ...)` in `src/main.rs`; banner + phase headers in `src/bin/concurrency_test.rs`.
- Do not add `println!` inside hot paths (`Pager::fetch_page`, `BLinkTree::search/scan_all`, `Page::serialize/deserialize`). Return `TitanError` and let the caller (`src/bin/server.rs`) format it.

## Comments

**When to Comment:**
- Numbered phase markers in demos/tests (`// 1. Create table`, `// 2. Prepopulate...` in `src/bin/concurrency_test.rs`; `// 0. Cleanup`, `// 1. Initialize Engine` in `src/main.rs`).
- Inline algorithm notes where logic is PoC/stubbed (`src/index/blink.rs`: `// 1. Move Right Logic (The B-link magic)`, `// Simple linear scan for PoC`, `// TODO: Implement interior search`).
- Field-level `//` notes on wire/layout-sensitive structs (`src/storage/page.rs`: `pub lsn: u64, // Log Sequence Number`, `high_key`/`right_link` B-Link notes).

**JSDoc/TSDoc:**
- Rust `///` doc comments are rare — only `BLinkTree::find_leaf` in `src/index/blink.rs` (`/// Finds the leaf page...`). Add `///` on every new `pub fn` describing args, return, and errors; keep `//` for internals.

## Function Design

**Size:** Keep functions under ~50 lines. Split `Executor::execute_statement` / `execute_query` in `src/sql/executor.rs` (already the largest at 218 lines file) by statement kind rather than growing the `match`.

**Parameters:** Borrow where possible (`&self`, `key: &[u8]`, `path: P: AsRef<Path>` generic in `Pager::open`); take ownership only when storing (`key: Vec<u8>, value: Vec<u8>` in `BLinkTree::insert`). Share engine state via `Arc<Pager>` + `Arc<RwLock<Catalog>>` (see `Executor::new` in `src/sql/executor.rs`, `BLinkTree::new` in `src/index/blink.rs`).

**Return Values:** Return `crate::error::Result<T>` for all fallible ops; `Option<Vec<u8>>` for point lookups (`BLinkTree::search`); plain values for infallible accessors (`BLinkTree::root_page_id() -> PageId`, `Executor::next_tx_id() -> u64`).

## Module Design

**Exports:** `pub mod` per subsystem in `src/lib.rs` (`error`, `storage`, `index`, `sql`, `catalog`) plus `pub use error::{Result, TitanError};`. Sub-modules re-export via `mod.rs` (`src/storage/mod.rs`). Items are `pub struct`/`pub fn` with private fields (`Pager { file, shards, total_pages }` in `src/storage/pager.rs`) and constructor `::new`/`::open`.

**Barrel Files:** `src/lib.rs` and each `mod.rs` act as barrels — add new modules there (e.g., `pub mod wal;` in `src/storage/mod.rs`) and import downstream via `crate::storage::...`, never via relative `super::` chains across subsystems.

---

*Convention analysis: 2026-10-07*
