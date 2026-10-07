# Testing Patterns

**Analysis Date:** 2026-10-07

## Test Framework

**Runner:**
- `cargo test` (Rust built-in `#[test]` harness). No separate runner in `Cargo.toml`.
- Config: `Cargo.toml` only (no `jest.config.*`, `vitest.config.*`, `.github/` workflows, `nextest` config). Edition 2021, crate `titan_db`.

**Assertion Library:**
- Standard `assert!` / `assert_eq!` macros (stdlib only — no `proptest`, `rstest`, `insta`, `mockall`/`mockito` in `Cargo.toml`).

**Run Commands:**
```bash
cargo test                 # Run all tests (currently zero #[test] functions in src/)
cargo test <name>          # Run matching tests
cargo test -- --nocapture  # Show println! output from tests/bins
```

## Test File Organization

**Location:**
- No `tests/` integration-test dir and no `#[cfg(test)] mod tests` modules exist in `src/` as of this audit. Verification is done via runnable binaries, not the test harness.
- Executable specs live in `src/bin/`: `src/bin/concurrency_test.rs` (MVCC isolation + disk persistence), `src/bin/ws_concurrency_test.rs` (WebSocket concurrency), `src/bin/dataset_populator.rs` (data seeding via WS), `src/bin/server.rs` (manual server under test).

**Naming:**
- Binary specs: `src/bin/<scenario>_test.rs` and `src/bin/<task>.rs`. Future unit tests must follow Rust convention: inline `#[cfg(test)] mod tests` in the same file, or `tests/<module>_test.rs` for integration tests.

**Structure:**
```
src/
├── lib.rs                    # crate root under test
├── error.rs
├── storage/pager.rs          # no inline tests (add #[cfg(test)] mod tests here)
├── storage/page.rs
├── index/blink.rs
├── catalog/mod.rs
├── sql/executor.rs
└── bin/
    ├── concurrency_test.rs   # cargo run --bin concurrency_test (executable spec)
    ├── ws_concurrency_test.rs# cargo run --bin ws_concurrency_test (needs server.rs running)
    ├── dataset_populator.rs  # cargo run --bin dataset_populator
    └── server.rs             # cargo run --bin server (manual fixture)
```

## Test Structure

**Suite Organization:**
```rust
// REQUIRED pattern for all new unit tests (not yet present — follow this exactly):
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use parking_lot::RwLock;
    use crate::catalog::Catalog;
    use crate::storage::pager::Pager;

    #[test]
    fn insert_then_search_returns_value() {
        let pager = Arc::new(Pager::open("test_insert.db").unwrap());
        // ... arrange / act / assert with assert_eq!
        std::fs::remove_file("test_insert.db").ok();
    }
}
```

**Patterns:**
- Setup pattern: build `Arc<Pager>` via `Pager::open(<temp path>)` + `Arc<RwLock<Catalog>>` + `Executor::new(pager, catalog)` (copy from `src/main.rs:17-19` and `src/bin/concurrency_test.rs:19-20`).
- Teardown pattern: `std::fs::remove_file(db_path).ok()` — bins already clean `titan_*.db` before/after runs; mirror that in tests with unique file names per test to avoid parallel-test collisions.
- Assertion pattern: `assert_eq!` on `ExecutionResult` / `scan_all` output; `assert!(result.is_err())` for duplicate-table and missing-page cases (`TitanError::PageNotFound`).

## Mocking

**Framework:** None (no mocking crates; no `#[mock]` usage in `src/`).

**Patterns:**
```rust
// What concurrency_test.rs does INSTEAD of mocking — use real components:
let p = Arc::new(Pager::open(db_path).expect("Failed to open Pager"));
let c = Arc::new(RwLock::new(Catalog::new()));
let executor = Executor::new(p.clone(), c.clone());
executor.execute("CREATE TABLE users (id INT, name TEXT, age INT)").unwrap();
```

**What to Mock:**
- Nothing yet. Prefer real `Pager` on a temp file plus real `BLinkTree`/`Catalog`/`Executor` — the engine is an embedded DB with cheap file-backed fixtures.

**What NOT to Mock:**
- Do NOT mock `Pager`, `Page::serialize/deserialize`, `BLinkTree`, or `Catalog` — persistence and MVCC semantics (`tx_read_ts`, `tx_expired`) are the behavior under test. Do NOT mock `sqlparser`; feed real SQL strings through `Executor::execute`.

## Fixtures and Factories

**Test Data:**
```rust
// From src/bin/concurrency_test.rs — the canonical seeding pattern to copy:
executor.execute("CREATE TABLE users (id INT, name TEXT, age INT)").unwrap();
for i in 0..50 {
    executor.execute(&format!(
        "INSERT INTO users (id, name, age) VALUES ({}, 'initial_{}', 20)", i, i
    )).unwrap();
}
// Snapshot ts BEFORE concurrent mutation, then spawn threads sharing Arc<Executor>:
// 50 concurrent inserts (ids 100..150) + concurrent updates/deletes, join handles,
// then assert snapshot read still sees pre-mutation rows and latest read sees all.
```

**Location:**
- No `tests/fixtures/` or factories exist. Keep SQL fixture strings inline in each test/bin. Large datasets go through `src/bin/dataset_populator.rs` against a running `src/bin/server.rs` (`ws://127.0.0.1:3030/ws`).

## Coverage

**Requirements:** None enforced (no `tarpaulin`/`llvm-cov` config, no CI gate, no `codecov.yml`).

**View Coverage:**
```bash
cargo test                                   # baseline: 0 tests — add unit tests first
cargo tarpaulin --out Html                   # optional, not configured; install separately if needed
```

## Test Types

**Unit Tests:**
- Scope: `Page::serialize` ↔ `deserialize` round-trip (`src/storage/page.rs`), `Pager::allocate_page/fetch_page/flush_page` persistence (`src/storage/pager.rs`), `BLinkTree::insert/search/delete/scan_all` MVCC visibility (`src/index/blink.rs`), `Executor` per-statement paths (`src/sql/executor.rs`). None exist — highest priority gap.

**Integration Tests:**
- Scope: executable-bin specs run with `cargo run --bin <name>`. `src/bin/concurrency_test.rs` is the model: file-backed open → seed 50 rows → snapshot ts → threaded inserts/updates/deletes → `handle.join().unwrap()` → assert snapshot isolation → assert `std::fs::metadata(db_path)` persists → re-`Pager::open` and `fetch_page(0)`. `src/bin/ws_concurrency_test.rs` covers the WS server path (`CREATE TABLE ws_items`, concurrent `INSERT`/`SELECT` over `tokio-tungstenite`).

**E2E Tests:**
- Manual only: `cargo run --bin server` (`src/bin/server.rs`, opens `titan_web.db` on `127.0.0.1:3030`) then `cargo run --bin dataset_populator` / `ws_concurrency_test`. No Playwright/Cypress harness; `web/` is static frontend assets, not test-driven.

## Common Patterns

**Async Testing:**
```rust
// No #[tokio::test] exists. When testing src/bin/server.rs paths, use this:
#[tokio::test]
async fn ws_insert_is_visible_to_next_query() {
    // spawn server fixture, connect_async("ws://127.0.0.1:3030/ws").await.expect(...),
    // write.send(Message::Text("INSERT ...".into())).await.unwrap();
    // let resp = read.next().await.unwrap().unwrap();
    // assert!(resp.to_text().unwrap().contains("..."));
}
// tokio with "full" features is already in Cargo.toml — just add #[tokio::test].
```

**Error Testing:**
```rust
#[test]
fn duplicate_table_errors() {
    // let executor = ...; // Executor::new setup as above
    executor.execute("CREATE TABLE users (id INT)").unwrap();
    let err = executor.execute("CREATE TABLE users (id INT)").unwrap_err();
    assert!(matches!(err, TitanError::Io(_)));
}
```

---

*Testing analysis: 2026-10-07*
