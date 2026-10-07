# Technology Stack

**Analysis Date:** 2026-10-07

## Languages

**Primary:**
- Rust 2021 edition - entire codebase (`src/`, `src/bin/`)

**Secondary:**
- JavaScript (vanilla, no framework) - browser admin UI WebSocket client (`web/index.html`)
- HTML/CSS (vanilla, no framework) - admin UI layout (`web/index.html`)

## Runtime

**Environment:**
- Rust stable toolchain, edition 2021 (`Cargo.toml:4`)
- Async runtime: Tokio full-feature build (`Cargo.toml:13`)

**Package Manager:**
- Cargo
- Lockfile: present (`Cargo.lock` committed)

## Frameworks

**Core:**
- warp 0.3 - HTTP static-file server + WebSocket upgrade (`src/bin/server.rs`)
- tokio 1.28 (features = ["full"]) - async runtime, `#[tokio::main]`, `tokio::spawn` (`src/bin/server.rs:10`, `src/bin/ws_concurrency_test.rs:25`)
- sqlparser 0.43 (PostgreSQL dialect) - SQL parsing via `PostgreSqlDialect` + `Parser` (`src/sql/executor.rs:1-3`)

**Testing:**
- No test framework declared in `Cargo.toml`. Concurrency/load checks are binaries, not `#[test]` suites: `src/bin/concurrency_test.rs`, `src/bin/ws_concurrency_test.rs`, `src/bin/dataset_populator.rs`

**Build/Dev:**
- Cargo (build, run, bins). Per-binary entry points under `src/bin/`
- Prebuilt artifact checked in: `bin/titan_db.exe`

## Key Dependencies

**Critical:**
- parking_lot 0.12 - `Arc<RwLock<...>>` shared engine state (`src/main.rs:4`, `src/storage/pager.rs:6`, `src/sql/executor.rs:6`, `src/bin/server.rs:3`)
- serde 1.0 (derive) + serde_json 1.0 - catalog/page derives and WebSocket JSON result encoding (`src/catalog/mod.rs:2`, `src/storage/page.rs:1`, `src/sql/mod.rs:3`, `src/bin/server.rs:53-54`)
- bincode 1.3 - on-disk page serialization (`src/storage/page.rs:72,80`, `src/error.rs:10`)
- thiserror 1.0 - typed `TitanError` enum (`src/error.rs:1-13`)
- futures 0.3 - `SinkExt`/`StreamExt` for WebSocket send/receive (`src/bin/server.rs:37`, `src/bin/dataset_populator.rs:2`)
- tokio-tungstenite 0.21 (+ tungstenite `Message`) - WS *clients* used by dev/load tools (`src/bin/dataset_populator.rs:3-4`, `src/bin/ws_concurrency_test.rs:3-4`)

**Infrastructure:**
- None - no ORM, no cloud SDK, no logging/metrics crate. File I/O via `std::fs` (`src/main.rs:2`)

## Configuration

**Environment:**
- No `.env` files, no dotenv, no config crate detected. No `std::env` reads detected in `src/`
- Database file paths are hardcoded: `titan_sql.db` (`src/main.rs:13,17`), `titan_web.db` (`src/bin/server.rs:13`)
- Server bind is hardcoded: `127.0.0.1:3030` (`src/bin/server.rs:17,33`); clients hardcode `ws://127.0.0.1:3030/ws` (`src/bin/dataset_populator.rs:18`, `src/bin/ws_concurrency_test.rs:8`)

**Build:**
- `Cargo.toml` (single package `titan_db` 0.1.0, no workspace, no build script, no features)
- Library + binaries layout: lib root `src/lib.rs`, default binary `src/main.rs`, extra binaries `src/bin/server.rs`, `src/bin/dataset_populator.rs`, `src/bin/concurrency_test.rs`, `src/bin/ws_concurrency_test.rs`
- No `rust-toolchain`, `Cross.toml`, `Dockerfile`, or CI config detected

## Platform Requirements

**Development:**
- Rust stable toolchain with Cargo; Windows-compatible (`bin/titan_db.exe` present, paths use relative `web` dir for `warp::fs::dir`)
- Run server from repo root so `web/` resolves: `cargo run --bin server`, then open `http://localhost:3030`

**Production:**
- Single self-contained binary + `web/` static dir sidecar; no external services required
- Embedded file-backed store (custom pager files `*.db` in CWD); binds loopback only - needs reverse proxy / bind change for remote access

---

*Stack analysis: 2026-10-07*
