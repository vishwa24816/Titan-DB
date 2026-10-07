---
phase: 01-production-grade-titandb
plan: 4
subsystem: wal-durability-catalog-recovery
tags: [titandb, wal, crash-recovery, catalog-persistence, crc32, mvcc-clock]
dependency_graph:
  requires: [01-01, 01-02]
  provides: [wal-append-commit-checkpoint, persisted-catalog, recovery-on-open, page-crc-framing, clock-restore]
  affects: [mvcc-plans, sql-plans, concurrency-tests]
tech-stack:
  added: []
  patterns: [aries-lite-analysis-redo-undo, steal-no-force, sidecar-catalog, crc-framed-pages, std-only-crc32]
key-files:
  created: [src/storage/wal.rs]
  modified: [src/storage/mod.rs, src/storage/page.rs, src/storage/pager.rs, src/catalog/mod.rs, src/index/blink.rs, src/txn/manager.rs, src/sql/executor.rs]
decisions:
  - "Sidecar .catalog file instead of reserved page 0 — no page-format coupling, independent writes"
  - "Std-only bitwise CRC32-IEEE — zero new deps, T-01-SC gate satisfied with nothing to install"
  - "Page frame CRC covers full padded content — catches bit flips in padding, not just body"
  - "TxManager clock restored from max_ts() scan on open — reopened snapshots see committed rows"
metrics:
  duration: ~45min
  completed: 2026-10-07
---

# Phase 1 Plan 4: WAL Durability + Catalog Recovery Summary

ARIES-lite WAL (append/commit/checkpoint/replay) with `sync_all`-before-ack commits, persisted sidecar catalog rebuilt on open, page-level CRC framing, and timestamp-clock restoration — restarts keep tables and committed rows, uncommitted txns vanish.

## Tasks Completed

| # | Name | Files | Result |
|---|------|-------|--------|
| 1 | WAL append + commit + checkpoint | src/storage/wal.rs (new), src/storage/mod.rs | Frame `magic u32 + LSN u64 + len u32 + crc32 u32 + bincode(LogRecord)`; `commit()` appends Commit + `sync_all` before ack; `checkpoint()` writes Begin/End + truncates prefix; 256 MiB size cap errors instead of OOM; CRC failure → `WalCorrupt`, never panic; no unwrap/expect; std-only CRC — **no new dependency, crc32fast gate closed with nothing to install** |
| 2 | Catalog persistence + recovery replay on open | src/catalog/mod.rs, src/storage/pager.rs, src/storage/page.rs, src/index/blink.rs, src/txn/manager.rs, src/sql/executor.rs | Sidecar `<db>.catalog` (bincode `Vec<PersistedTable>` + LSN, synced on CREATE); `Executor::new` recovers (catalog rebuild via `BLinkTree::open_existing` + `Wal::replay` validation); writes WAL-logged with commit+sync; `checkpoint()` API (flush_all + persist + WAL truncate); `wal_tests` 3/3 green |

## Verification

- `cargo build` — clean, zero warnings
- `cargo test --lib` — 12 passed (incl. 2 new WAL unit tests)
- `cargo test --test wal_tests` — 3/3 green: `wal_commit_durable`, `wal_uncommitted_gone`, `wal_corrupt_frame_errors`
- `cargo test --test blink_tests` — 3/3 green; `--test mvcc_tests` — 5/5 green; `--test concurrency_tests` — 1/1 green
- `sql_tests` 6 failures are pre-existing intended gaps (WHERE/JOIN/window evaluation = later plans' scope, per 01-02 summary) — unchanged behavior, verified `sql_where` fails on missing filtering, not on errors
- Kill-9-style acceptance: `wal_commit_durable` commits, drops without checkpoint, reopens, row present; uncommitted (buffered BEGIN, no COMMIT) absent after reopen; catalog survives restart

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Windows `sync_all` on read-only handle → Access Denied**
- **Found during:** Task 2 (CREATE TABLE returned `Io PermissionDenied`)
- **Issue:** Catalog `persist()` re-opened the sidecar with `File::open` (GENERIC_READ); Windows `FlushFileBuffers` requires write access
- **Fix:** Open sidecar with `read(true).write(true)` for the sync step (one-line change in `src/catalog/mod.rs`)

**2. [Rule 2 - Missing critical] TxManager clock reset hid committed rows after reopen**
- **Found during:** Task 2 (`wal_commit_durable` got empty ResultSet after reopen)
- **Issue:** Fresh `TxManager` starts at 1; persisted versions stamped with higher ts were invisible to new snapshots
- **Fix:** `BLinkTree::max_ts()` (raw-record timestamp scan) + `TxManager::restore_clock()` (monotonic CAS bump) wired into `Executor::recover()`

**3. [Rule 1 - Bug] Page CRC missed padding flips; bincode could panic on corrupt lengths**
- **Found during:** Task 2 (`wal_corrupt_frame_errors` accepted corrupt frame)
- **Issue:** CRC covered body only (flips in zero padding undetected); corrupt inner vec-lengths could panic bincode (capacity overflow) instead of erroring
- **Fix:** CRC covers full padded page content; body-length cap rejects absurd `len` before deserialize; `catch_unwind` maps bincode panics → `CorruptPage` (T-01-04)

**4. [Rule 1 - Bug] Serialize truncated oversize bodies (broke RowTooLarge + deadlocked)**
- **Found during:** Full-suite regression (`pager_oversize_flush_rejected` hung 60s+)
- **Issue:** First version of framed `serialize` used `resize()` which truncates: oversize pages flushed successfully (contract broken) and reached a write-lock while the test held a read-lock (parking_lot write-under-read deadlock)
- **Fix:** `serialize` returns `RowTooLarge` for bodies over `PAGE_SIZE - 12`; flush returns before any write, matching the 01-01 contract

## Known Stubs / Limitations

- **Row-level WAL redo deferred:** data pages flush per-op (STEAL + NO-FORCE), so `Wal::replay` currently validates frames + filters loser txns; physical re-application of committed records from WAL alone (needed if per-op flush is ever removed) is a Wave-3 upgrade. No test depends on it.
- **WAL table scoping in `commit_tx`:** uses the first catalog table name (single-table prototype; `apply_op` resolves trees the same way). Multi-table WAL redo scoping rides with the executor rewrite.
- **No new deps:** `crc32fast` deliberately NOT added — std-only bitwise CRC32-IEEE. RESEARCH audit gate satisfied vacuously; no crates.io verification needed.

## Threat Flags

None beyond mitigations already in the threat model: T-01-04 (CRC per WAL frame + per page → halt replay/error, panic-proof decode), T-01-05 (checkpoint truncation + 256 MiB WAL cap → error not OOM), T-01-SC (no package installed).

## Self-Check: PASSED

- `src/storage/wal.rs` created and present; all six modified files present
- No git repo in working directory — commits skipped per executor instructions
- `cargo build` clean; `wal_tests` 3/3, lib 12/12, blink 3/3, mvcc 5/5, concurrency 1/1 green
