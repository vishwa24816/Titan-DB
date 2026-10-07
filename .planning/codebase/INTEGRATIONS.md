# External Integrations

**Analysis Date:** 2026-10-07

## APIs & External Services

**None.** Zero third-party APIs, SaaS, or external services. No HTTP clients, no SDK imports, no cloud clients detected in `src/`.

**Self-hosted interfaces (this repo IS the server):**
- Static file server - serves `web/` admin UI via `warp::fs::dir("web")` (`src/bin/server.rs:20`)
- WebSocket API - `ws://127.0.0.1:3030/ws`, plain-text SQL in, JSON `ExecutionResult` out (`src/bin/server.rs:24-29,36-63`, client in `web/index.html:155-179`)
- Internal dev/load clients - `src/bin/dataset_populator.rs`, `src/bin/ws_concurrency_test.rs` connect to the above WS endpoint

## Data Storage

**Databases:**
- Custom embedded file-backed store (not Postgres/SQLite - only SQL *dialect* is Postgres-compatible via `sqlparser`)
  - Pager + page files in CWD: `titan_sql.db` (CLI demo, `src/main.rs:13,17`), `titan_web.db` (server, `src/bin/server.rs:13`)
  - Serialization: `bincode` (`src/storage/page.rs:72,80`); schema types: `serde` derives (`src/catalog/mod.rs:2`)
  - Client: direct in-process `Pager`/`Catalog`/`Executor` (`src/main.rs:17-19`, `src/bin/server.rs:13-15`)

**File Storage:**
- Local filesystem only (`std::fs` page files + `web/` static dir). No S3/object storage

**Caching:**
- None (no Redis/memcached/in-process cache crate)

## Authentication & Identity

**Auth Provider:**
- None. No auth, no sessions, no tokens
  - Implementation: open loopback server - anyone who can reach `127.0.0.1:3030` can execute arbitrary SQL via WS (`src/bin/server.rs:33,48-52`)

## Monitoring & Observability

**Error Tracking:**
- None (no Sentry/OTel)

**Logs:**
- `println!`/`eprintln!` to stdout/stderr only: startup banner + per-query log (`src/bin/server.rs:17,49`), WS error lines (`src/bin/server.rs:43,58`)

## CI/CD & Deployment

**Hosting:**
- None declared. Local-only loopback binary; deployable as single binary + `web/` dir

**CI Pipeline:**
- None detected (no GitHub Actions / Jenkins / Dockerfile)

## Environment Configuration

**Required env vars:**
- None. No env reads, no secret files referenced

**Secrets location:**
- Not applicable - there are no credentials, tokens, or connection strings anywhere in the codebase

## Webhooks & Callbacks

**Incoming:**
- None. Only routes are `GET /...` static files and `GET /ws` upgrade (`src/bin/server.rs:20-31`)

**Outgoing:**
- None. The tungstenite clients make *outbound WS connections to itself* (localhost dev tooling), not to external systems (`src/bin/dataset_populator.rs:18`, `src/bin/ws_concurrency_test.rs:26,39,52`)

---

*Integration audit: 2026-10-07*
