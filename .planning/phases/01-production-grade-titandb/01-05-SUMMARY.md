---
phase: 01-production-grade-titandb
plan: 5
subsystem: sql-mvcc
tags: [titandb, mvcc, snapshot-isolation, typed-encoding, vacuum]
dependency_graph:
  requires: [01-01, 01-02]
  provides: [typed-tuple-encoding, txmanager-si, executor-tx-threading, vacuum-gc]
  affects: [01-06, ws-tx-protocol]
tech-stack:
  added: []
  patterns: [length-prefixed-tuples, buffered-write-txns, first-committer-wins, gc-horizon]
key-files:
  created: [src/sql/encoding.rs, src/txn/manager.rs, src/txn/mod.rs, tests/mvcc_tests.rs]
  modified: [src/sql/executor.rs, src/catalog/mod.rs, src/index/blink.rs, src/lib.rs, src/sql/mod.rs, src/storage/wal.rs]
decisions: []
metrics:
  duration: ~40min
  completed: 2026-10-07
---

# Phase 1 Plan 5: Typed Encoding + MVCC Transactions Summary

Length-prefixed typed tuple encoding (`src/sql/encoding.rs`) replacing the `|||` delimiter protocol, plus a snapshot-isolated `TxManager` (`src/txn/manager.rs`) with first-committer-wins validation, buffered-write transactions threaded through the executor, and horizon-based vacuum/GC.

## Tasks Completed

| # | Name | Files | Result |
|---|------|-------|--------|
| 1 | Typed tuple encoding replaces `\|\|\|` | src/sql/encoding.rs, src/sql/executor.rs, src/catalog/mod.rs | encode/decode with tag+len+payload per col; all executor delimiter sites migrated; 3 unit tests green |
| 2 | TxManager + executor tx threading + vacuum | src/txn/manager.rs, src/sql/executor.rs, src/index/blink.rs, tests/mvcc_tests.rs | BEGIN/COMMIT/ROLLBACK, buffered writes, first-committer-wins, vacuum; 5 mvcc tests + 4 manager unit tests green |

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Fail-first mvcc test was unpassable as written**
- **Found during:** Task 2
- **Issue:** `mvcc_first_committer_wins` hardcoded `let validated = false` + assert, failing regardless of implementation
- **Fix:** Rewrote `tests/mvcc_tests.rs` to assert real behavior (TxManager commit returns `TxConflict` for the loser); added commit round-trip + vacuum tests
- **Files modified:** tests/mvcc_tests.rs

**2. [Rule 1 - Bug] `wal.rs` test borrowck error blocked `cargo test --lib` verification**
- **Found during:** Task 2 verification
- **Issue:** `bytes[bytes.len() / 2] ^= 0xFF` (E0502, parallel-wave file) broke lib-test compilation
- **Fix:** Hoisted `let mid = bytes.len() / 2;` — test-only, no behavior change
- **Files modified:** src/storage/wal.rs

**3. [Rule 1 - Bug] Bogus byte-absence assertion in own encoding test**
- **Found during:** Task 1 verification
- **Issue:** Asserted encoded bytes never contain `|||` — false for length-prefixed payloads carrying that text
- **Fix:** Removed assertion; round-trip equality is the correct property

**4. [Rule 2 - Missing] `DataType` lacked Float/Bytes/Date for typed decode**
- **Found during:** Task 1
- **Issue:** Catalog had only Integer/Text/Boolean but encoding spec requires Float/Bool/Bytes/Date tags
- **Fix:** Added `Float`, `Bytes`, `Date` variants; extended CreateTable type mapping

## Verification

- `cargo build --lib` — passes
- `cargo test --lib` — 12 passed, 0 failed (incl. 3 encoding + 4 txn manager unit tests)
- `cargo test --test mvcc_tests` — 5 passed, 0 failed
- `|||` in src/: only test string literals remain (delimiter protocol deleted); `GLOBAL_TX_ID` removed (0 matches)
- SI documented as non-serializable (write-skew possible) per T-01-06

## Threat Flags

None — mitigations landed as planned: first-committer-wins -> TxConflict (T-01-06), vacuum horizon + 128-version chain cap -> RowTooLarge (T-01-07), no new deps (T-01-SC).

## Self-Check: PASSED

- src/sql/encoding.rs, src/txn/manager.rs, src/txn/mod.rs present
- Executor BEGIN/COMMIT/ROLLBACK + buffered writes + overlay reads verified by tests
- No git repo — commits skipped per executor instructions
