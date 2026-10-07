# Phase 1: Production-Grade TitanDB - Research

**Researched:** 2026-10-07
**Domain:** Embedded MVCC relational engine in Rust (B-link tree, WAL, SQL)
**Confidence:** MEDIUM

## Summary

TitanDB is a single-crate Rust 2021 embedded DB (sqlparser 0.43, parking_lot 0.12, tokio/warp WS server, bincode 1.3 pages). Phase 1 closes the prototype→production gap: full Lehman-Yao B-link splits/merges, snapshot-isolation MVCC with tx manager + vacuum, ARIES-style WAL + LSN + checkpoint/recovery, typed tuple encoding, full WHERE/JOIN/aggregate/window evaluation, positional I/O + smaller pages, single-exe packaging, and a real `cargo test` suite.

**Primary recommendation:** Keep current crates; add no new runtime deps except optionally `rust-embed` [ASSUMED] for exe bundling — implement WAL (append-only CRC log + fuzzy checkpoint), TxManager (AtomicU64 clock, active-tx table, first-committer-wins, version-chain GC), B-link split/merge with latch-crabbing, and a Volcano-style executor over sqlparser AST, all behind extended `TitanError`, verified by split/merge, MVCC-matrix, crash-recovery, and SQL-semantics tests.

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions
- D-01: Full B+tree with Lehman-Yao splits (and merges), high_key/right_link invariants, split tests first (`src/index/blink.rs:26-71`).
- D-02: Fix scan_all to newest-first per-key (match search semantics); oversize-page → error + split/overflow, never corrupt neighbors.
- D-03: Replace std Mutex unwrap with parking_lot + TitanError::LockError; fix fetch_page TOCTOU + fresh-DB UnexpectedEof.
- D-04: Full BEGIN/COMMIT/ROLLBACK, snapshot isolation, read-ts pinning, conflict detection, background vacuum/GC.
- D-05: Transaction-capable WebSocket: session-pinned tx context + fast tx channel in addition to single-statement WS.
- D-06: WAL + LSN + sync_all on commit + checkpoint + crash recovery replay; persisted catalog, rebuild on open.
- D-07: Full functions: numeric, string, datetime, CASE/COALESCE/NULLIF, operators, aggregates, window (ROW_NUMBER/RANK/OVER/PARTITION BY).
- D-08: Indexing + JOINs (inner/left/right, predicate pushdown WHERE id=? → search), WHERE/ORDER BY/LIMIT from AST, honest errors for unsupported syntax.
- D-09: Length-prefixed typed tuple encoding replacing `|||`-delimited strings, decoded by column type.
- D-10: Single-exe packaging with pgAdmin-style frontend, DB path via CLI arg/env, token auth + WS caps/rate limits before non-local exposure.
- D-11: Multicore: positional I/O or per-thread handles, lock-free cache-hit reads, group-commit/background flusher; revisit 64 KiB page size (8/16 KiB) with benchmarks.
- D-12: Success = cargo test suite (pager crash-consistency, MVCC matrix, SQL semantics, concurrency); convert concurrency_test.rs assertions (150 ops, SI, persistence) to automated tests.
### the agent's Discretion
- Exact WAL record format, final page size, WS tx wire protocol, exe bundling (embed web/ vs sidecar) — researcher/planner decide, guided by benchmarks.
### Deferred Ideas (OUT OF SCOPE)
- None.
</user_constraints>

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| B-link storage + pager + WAL | Storage (embedded lib) | — | Durability/concurrency live in `src/storage`, `src/index` |
| MVCC tx manager, GC | Backend lib (TxManager) | — | Timestamp oracle + visibility is engine-global, not per-connection |
| SQL eval (WHERE/JOIN/aggr/window) | Backend lib (executor) | — | Volcano operators over AST; no separate API tier |
| WS tx sessions, auth, rate limits | Server adapter (`src/bin/server.rs`) | — | Async boundary; core stays Sync + `spawn_blocking` |
| Web UI grid + tx manager | Static frontend (`web/`) | CDN/static | Served by warp or embedded in exe |

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| sqlparser | 0.43 (pinned) | PostgreSQL-dialect parsing | Already integrated; AST (`Statement`, `Expr`, `Select`, window specs) is the eval input [VERIFIED: Cargo.toml] |
| parking_lot 0.12 | 0.12 (pinned) | RwLock/Mutex without poisoning | Existing standard; `RwLock` upgradable guards enable latch-crabbing [VERIFIED: Cargo.toml] |
| thiserror 1.0 | 1.0 (pinned) | TitanError extension | Extend enum, never unwrap on hot paths [VERIFIED: Cargo.toml] |
| bincode 1.3 + serde | pinned | Page/WAL serde | Keep for pages; WAL uses explicit u32-len framing + CRC32 [VERIFIED: Cargo.toml] |
| tokio full + warp 0.3 + futures/tungstenite | pinned | WS server | Keep; add `spawn_blocking` + session map [VERIFIED: Cargo.toml] |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `crc32fast` [ASSUMED] | latest | WAL frame checksums | WAL record integrity |
| `rust-embed` [ASSUMED] | latest | Embed `web/` into exe | Single-exe packaging (alt: sidecar dir) |
| `chrono` [ASSUMED] | latest | date/time fn evaluation | D-07 datetime functions |
| `regex` [ASSUMED] | latest | LIKE/pattern string fns | Only if LIKE required |

**Installation:**
```bash
cargo add crc32fast rust-embed chrono regex
```

## Package Legitimacy Audit

Slopcheck unavailable in this env — all new packages `[ASSUMED]`; planner must gate each install behind `checkpoint:human-verify`, confirm on crates.io + docs.rs, run `cargo search <pkg>`, and check for postinstall/build scripts (Rust: inspect `build.rs`).

| Package | Registry | Age | Downloads | Source Repo | slopcheck | Disposition |
|---------|----------|-----|-----------|-------------|-----------|-------------|
| crc32fast | crates.io | ~9 yrs [ASSUMED] | high [ASSUMED] | github.com/srijs/rust-crc32fast [ASSUMED] | n/a | Flagged — verify before use |
| rust-embed | crates.io | ~7 yrs [ASSUMED] | high [ASSUMED] | github.com/pyros2097/rust-embed [ASSUMED] | n/a | Flagged — verify before use |
| chrono | crates.io | ~10 yrs [ASSUMED] | very high [ASSUMED] | github.com/chronotope/chrono [ASSUMED] | n/a | Flagged — verify before use |
| regex | crates.io | ~10 yrs [ASSUMED] | very high [ASSUMED] | github.com/rust-lang/regex [ASSUMED] | n/a | Flagged — verify before use |

## Architecture Patterns

### System Architecture Diagram

```
Client (browser web/index.html: grid + tx manager)
  │ WS JSON {session, tx_op|sql}  ← token auth, caps/rate limits
  ▼
Server adapter (warp: static + /ws; session→TxContext map; spawn_blocking)
  │ Sync Executor (parse sqlparser AST → plan Volcano ops → eval)
  ▼
TxManager (AtomicU64 clock, active-tx table, read-ts pin, write/write conflict → TitanError::TxConflict, GC horizon)
  │ visibility filter begin<=ts<end on version chains
  ▼
BLinkTree per table (latch-crab descent + move-right on high_key/right_link; split bottom-up; merge/coalesce on delete)
  │ fetch/alloc via Pager
  ▼
Pager (16 shards, page cache) + WAL (append CRC frame → fsync/sync_all on commit; fuzzy checkpoint; recovery Analysis/Redo/Undo)
  ▼ File: *.db (8/16 KiB pages) + *.wal + catalog page/sidecar
```

### Recommended Project Structure
```
src/
├── error.rs            # extended TitanError
├── storage/pager.rs    # shards + positional I/O + sync_all + TOCTOU fix
├── storage/page.rs     # PageHeader{lsn,high_key,right_link}+NodeContent typed records
├── storage/wal.rs      # NEW: append/replay/checkpoint (LSN, CRC)
├── index/blink.rs      # descent/split/merge/move-right, newest-first scan
├── txn/manager.rs      # NEW: begin/commit/rollback, snapshot, conflicts, vacuum
├── sql/executor.rs     # plan+eval: WHERE/JOIN/aggr/window, pushdown, honest errors
├── sql/encoding.rs     # NEW: length-prefixed typed tuples
├── catalog/mod.rs      # persisted catalog
└── bin/server.rs       # WS tx protocol + auth + limits + spawn_blocking
```

### Pattern 1: Lehman-Yao B-link split (latch-crabbing)
**What:** Readers never latch (move-right via high_key compensates for concurrent splits); writers crab down holding ≤3 page latches, split bottom-up: alloc sibling, redistribute, set sibling high_key/right_link = old values, set orig high_key=split key + right_link=sibling, flush sibling first then orig, then parent insert. [CITED: db.cs.cmu.edu/papers/1981/lehman-tods1981.pdf; CITED: pages.cs.wisc.edu/~yxy/cs764-f21/slides/L13.pdf]
**When:** D-01/D-02. Merges: delete leaves tombstone; coalesce/merge underfull sibling + parent key removal (ARIES/IM style) [ASSUMED]; simpler alternative: mark-empty + background compaction [ASSUMED].
**Example:**
```rust
// sketch: descent with move-right; writers use write guards parent→child (crabbing)
let mut node = root(fetch read);
loop {
  while key > node.high_key { node = node.right_link(fetch read); } // move-right [CITED above]
  if node.is_leaf { break; }
  child = node.child_for(key); drop parent guard if child deemed safe (not full) // crabbing
}
```

### Pattern 2: MVCC snapshot isolation (Hekaton/Turso shape)
**What:** Global AtomicU64 clock; BEGIN pins read_ts; writes create versions tagged tx_id invisible to others; COMMIT takes commit_ts, validates write-set (first-committer-wins: abort if newer committed version since read_ts → `TxConflict`), stamps versions; ROLLBACK discards; vacuum drops versions with end < oldest-active-ts. [CITED: github.com/tursodatabase/turso/blob/main/docs/agent-guides/mvcc.md; CITED: github.com/joaoh82/rust_sqlite concurrent-writes-plan]
**Example:** visibility `begin <= read_ts < end`; readers never block writers. GC horizon = min active read_ts [CITED: turso mvcc guide].

### Pattern 3: ARIES-lite WAL + fuzzy checkpoint
**What:** WAL rule: log record forced before data page; commit = WAL commit record + `sync_all` (NO-FORCE data pages allowed). Each record has monotonic LSN; pages carry pageLSN. Recovery = Analysis (active txns, dirty pages from checkpoint) → Redo (repeat history from earliest dirty) → Undo (logical undo of losers with CLRs). [CITED: postgresql.org/docs WAL-internals; CITED: cmu 15445 lecture 21 recovery notes]. Titan scope: physiological row-level log (table,key,before/after image,tx) + commit/abort + checkpoint(begin/end) records [ASSUMED].
**Record sketch:** `frame = magic|u64 LSN|u32 len|u32 crc32|bincode(LogRecord)`; `LogRecord = {Put{..}, Delete{..}, Commit{tx}, Abort{tx}, CheckpointBegin/End{active,dirty}}`.

### Pattern 4: Volcano executor over sqlparser AST
**What:** Lower `Statement::Query/Insert/Update/Delete` structurally: Scan (pushdown `WHERE pk=?` → `search`, else range `scan_all` + filter) → Filter(Expr eval incl. CASE/COALESCE/operators) → Join (nested-loop + hash join for equi-join; support inner/left/right) → Aggregate (GROUP BY hash + COUNT/SUM/AVG/MIN/MAX) → Window (partition sort + ROW_NUMBER/RANK) → Sort/Limit. Unsupported AST → `TitanError::UnsupportedSql`. sqlparser 0.43 AST shapes from training only — verify variants against docs.rs during planning [ASSUMED].

### Pattern 5: Typed tuple encoding
**What:** `col_count u32 + per-col (tag u8 + len-prefixed payload)`; tags Null/Int64/Float64/Bool/Text/Bytes/Date; decode by catalog `DataType`; NULL bitmap optional. Fixes `|||` escaping + type fidelity [ASSUMED — standard length-prefix design].

### Anti-Patterns to Avoid
- **Full-page `Mutex<File>` + `seek+read`:** cursor shared → contention + TOCTOU; use positional I/O instead.
- **Silent fake DDL/silent 0-row:** always return `UnsupportedSql` for unhandled AST.
- **Oldest-first scan dedup:** must be newest-first per key to match `search`.
- **Blocking engine inside warp handler:** wrap `executor.execute` in `spawn_blocking`.
- **Oversize row truncation:** return `RowTooLarge`, never corrupt neighbors.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| SQL parsing | Custom parser | sqlparser 0.43 (pinned) | Dialect, precedence, window syntax edge cases |
| Checksums | Custom hash | crc32fast [ASSUMED] | Standard, hardware-friendly |
| Datetime arithmetic | Custom calendar | chrono [ASSUMED] | Leap/DST/TZ pitfalls |
| Static embedding | Custom linker script | rust-embed [ASSUMED] or sidecar | Cache + fallback complexity |
| Auth tokens | Custom crypto | Constant-time compare + OS RNG token [ASSUMED] | Timing attacks |

## Common Pitfalls

### Pitfall 1: Windows positional I/O moves the cursor
**What:** `std::os::windows::fs::FileExt::seek_read/seek_write` take `&self` but DO move the file cursor (unlike Unix `pread/pwrite`). [CITED: doc.rust-lang.org FileExt; CITED: docs.rs/file_offset notes]. Concurrent seek_read + seek_write race on cursor.
**Avoid:** Guard each positional op with a small `Mutex` around file handle, OR open per-thread `File` handles via `try_clone` (each has own cursor) [ASSUMED], OR use `Overlapped` I/O. Recommended: keep one `File` + dedicated `io_lock: Mutex<()>` only around the syscall; cache-hit reads skip file entirely. Optionally adopt `file_offset` crate's portable `read_offset/write_offset` [ASSUMED].

### Pitfall 2: 64 KiB pages + flush-per-op kills latency
**What:** 64 KiB write amplification + `flush` per mutation. **Avoid:** 8/16 KiB pages (verify bincode row fit + split threshold with benchmarks), group-commit (batch WAL fsync per N commits/10ms window), background flusher for dirty pages [ASSUMED].

### Pitfall 3: B-link forgetting move-right after split
**What:** Descent lands on pre-split node → wrong leaf. **Avoid:** always `while key > high_key { go right_link }` at every level + re-check after latching [CITED: Lehman-Yao paper].

### Pitfall 4: MVCC write-skew / lost update
**What:** SI permits write skew; naive commit loses updates. **Avoid:** first-committer-wins validation on write-set; document SI≠serializable [CITED: turso mvcc guide].

### Pitfall 5: Unbounded version chains / WAL growth
**What:** No GC → memory/disk blowup. **Avoid:** vacuum horizon + checkpoint truncation; Turso MVCC itself lacks GC — Titan must implement it [CITED: turso mvcc limitations].

### Pitfall 6: sqlparser version drift
**What:** 0.43 AST variants differ from training memory. **Avoid:** verify `Expr`, `Select`, window AST on docs.rs during planning [ASSUMED].

## Code Examples

### Positional read on Windows (std only)
```rust
// Source: https://doc.rust-lang.org/std/os/windows/fs/trait.FileExt.html
use std::os::windows::fs::FileExt;
let _io = self.io_lock.lock(); // cursor moves — serialize syscalls
file.seek_read(&mut buf, page_id * PAGE_SIZE as u64)?;
```

### WAL commit (STEAL + NO-FORCE)
```rust
// wal.append(Put{..})?; wal.append(Commit{tx})?; wal.sync_all()?; // durable before ack
// pages flushed later by background flusher / checkpoint
```

### Error extension (thiserror)
```rust
#[derive(thiserror::Error, Debug)]
pub enum TitanError {
  #[error("io: {0}")] Io(#[from] std::io::Error),
  #[error("tx conflict")] TxConflict,
  #[error("unsupported sql: {0}")] UnsupportedSql(String),
  #[error("row too large: {0} bytes")] RowTooLarge(usize),
  #[error("lock error")] LockError,
  #[error("corrupt page {0}: {1}")] CorruptPage(u64, String),
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Single-leaf + scan root only | Full B-link split/merge + interior descent | This phase | Tables exceed one page |
| `|||` strings | Typed length-prefixed tuples | This phase | NULLs, types, escaping |
| GLOBAL_TX_ID per statement | TxManager SI + validation + vacuum | This phase (Hekaton/Turso shape) | Real transactions |
| Flush-per-op, 64 KiB | WAL + group commit + 8/16 KiB + bg flusher | This phase | Durability + throughput |
| Eyeball binaries | `cargo test` suites | This phase | Regression safety |

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | crc32fast/rust-embed/chrono/regex current + suitable | Stack | Minor — planner verifies on crates.io |
| A2 | sqlparser 0.43 AST shapes (Select/Expr/Window) | Patterns | Medium — verify on docs.rs before codegen |
| A3 | try_clone per-thread handles + file_offset behavior | Pitfalls | Low — fallback to io_lock serialization |
| A4 | Optimal page size 8/16 KiB; group-commit window | Pitfalls | Low — benchmarks decide |
| A5 | Merge/coalesce design (ARIES/IM style) | Patterns | Medium — tests pin invariants |

## Open Questions

1. **Embed vs sidecar for web/** — rust-embed simplifies exe but complicates dev iteration; recommend embed-with-debug-fallback, planner benchmarks. [ASSUMED]
2. **WS tx wire format** — propose `{session, op: begin|stmt|commit|rollback, sql?}` + `{tx_id, status, rows|error}`; planner finalizes.
3. **Isolation scope** — SI only (documented, no SSI); write-skew accepted.

## Environment Availability

SKIPPED (embedded lib + cargo-only; no external services). Planner to verify `cargo --version` at plan time.

## Validation Architecture

| Property | Value |
|----------|-------|
| Framework | Rust built-in `cargo test` (no extra harness) |
| Config file | none — see Wave 0 |
| Quick run | `cargo test --lib` |
| Full suite | `cargo test` |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|--------------|
| D-01 | Split grows height, move-right finds keys | unit | `cargo test blink_split` | ❌ Wave 0 |
| D-01 | Merge/coalesce preserves order | unit | `cargo test blink_merge` | ❌ Wave 0 |
| D-02 | scan newest-first matches search | unit | `cargo test scan_visibility` | ❌ Wave 0 |
| D-04 | SI matrix: no dirty reads, first-committer-wins, rollback invisible | integration | `cargo test mvcc_` | ❌ Wave 0 |
| D-06 | Kill -9 mid-txn → committed durable, uncommitted gone | integration | `cargo test wal_recovery` | ❌ Wave 0 |
| D-08 | WHERE/JOIN/aggr/window golden semantics | unit | `cargo test sql_` | ❌ Wave 0 |
| D-11 | Page-size + group-commit bench | bench | `cargo test --release -- --ignored bench_` | ❌ Wave 0 |
| D-12 | 150-op concurrency + persistence | integration | `cargo test concurrency_` | ❌ Wave 0 |

### Sampling Rate
- Per task commit: `cargo test --lib`
- Per wave merge: `cargo test`
- Phase gate: full suite green before verify-work.

### Wave 0 Gaps
- [ ] `tests/blink_tests.rs`, `tests/mvcc_tests.rs`, `tests/wal_tests.rs`, `tests/sql_tests.rs`
- [ ] `src/storage/wal.rs`, `src/txn/manager.rs`, `src/sql/encoding.rs` scaffolds

## Security Domain

| ASVS Category | Applies | Standard Control |
|---------------|---------|------------------|
| V2 Authentication | yes | Bearer token (env/arg), constant-time compare [ASSUMED] |
| V3 Session Management | yes | WS session→tx map, idle timeout, explicit rollback on drop |
| V4 Access Control | yes | Localhost bind default; explicit opt-in for remote |
| V5 Input Validation | yes | sqlparser parse + typed bind; WS msg caps (e.g., 1 MiB) + rate limits |
| V6 Cryptography | no | No custom crypto; RNG token only |

| Pattern | STRIDE | Mitigation |
|---------|--------|------------|
| Unauth remote WS SQL exec | Elevation | Token auth + localhost default |
| WS memory exhaustion | DoS | Message cap + per-conn rate limit + spawn_blocking |
| Corrupt WAL/page accepted | Tampering | CRC32 frame check → CorruptPage error, recovery halt |

## Sources

### Primary (HIGH)
- Lehman & Yao 1981 original (db.cs.cmu.edu/papers/1981/lehman-tods1981.pdf) — B-link split/link/high_key
- UWisc B-link slides (pages.cs.wisc.edu/~yxy/cs764-f21/slides/L13.pdf) — lock coupling vs B-link
- Berkeley tree-CCR notes (dsf.berkeley.edu/jmh/cs262b/treeCCR.html) — latch vs lock, insert algorithm
- PostgreSQL WAL-internals docs — LSN, checkpoint, pg_control/REDO
- CMU 15-445 recovery notes — ARIES Analysis/Redo/Undo, pageLSN/flushedLSN
- Turso MVCC guide (github.com/tursodatabase/turso mvcc.md) — row-version shape, checkpoint, no-GC warning
- rust std FileExt docs (doc.rust-lang.org) — Windows seek_read/seek_write cursor semantics

### Secondary (MEDIUM)
- sqlrite concurrent-writes plan (joaoh82/rust_sqlite) — Turso-cross-checked MVCC commit/validation design
- stoolap/tikv MVCC docs.rs — registry/version-store shapes

### Tertiary (LOW)
- Crate ages/download claims — unverified, planner to confirm on crates.io

## Metadata

**Confidence breakdown:**
- Standard stack: MEDIUM — pinned deps verified in Cargo.toml; new crates assumed
- Architecture: MEDIUM — B-link/ARIES/MVCC verified against primary sources; Titan AST mapping assumed
- Pitfalls: HIGH — Windows cursor + GC + SI anomalies sourced

**Research date:** 2026-10-07
**Valid until:** ~30 days (stable domain; recheck sqlparser/crate versions at plan time)
