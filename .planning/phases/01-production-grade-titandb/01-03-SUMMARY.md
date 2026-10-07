---
phase: 01-production-grade-titandb
plan: 3
subsystem: blink-tree
tags: [titandb, b-link-tree, lehman-yao, mvcc-scan, splits]
dependency_graph:
  requires: [extended-titanerror, hardened-pager, test-harness]
  provides: [multi-level-blink-tree, newest-first-scan, row-too-large-guard]
  affects: [mvcc-plans, wal-plans, sql-plans]
tech-stack:
  added: []
  patterns: [b-link-move-right, sibling-before-orig-flush, hop-cap-corruptpage]
key-files:
  created: []
  modified: [src/index/blink.rs, src/storage/pager.rs]
decisions: []
metrics:
  duration: ~40min
  completed: 2026-10-07
---

# Phase 1 Plan 3: Full B-Link Tree Summary

Full B-link tree in `src/index/blink.rs`: interior descent with per-level move-right, Lehman-Yao leaf/interior splits (sibling-flushed-first, root-split creates new Interior root), best-effort delete-coalesce, newest-first leaf-chain `scan_all` matching `search()`, single-record `RowTooLarge` probe before touching neighbors, LSN stamped on every write, hop-capped right-chase → `CorruptPage`.

## Tasks Completed

| # | Name | Files | Result |
|---|------|-------|--------|
| 1 | Interior descent + Lehman-Yao split | src/index/blink.rs, src/storage/page.rs (LSN wiring in blink.rs) | `child_for`/`descend`/`split_leaf`/`split_interior`/`insert_separator` landed; blink 3/3 green |
| 2 | Merge/coalesce + newest-first scan + overflow | src/index/blink.rs | `try_coalesce`, chain `scan_all` via `fold_records`, probe guard; blink + concurrency green |

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Fixed `Pager::open` use-after-move (parallel-wave breakage)**
- **Found during:** Task 2 verification (`cargo test` failed to compile lib)
- **Issue:** A concurrent wave added a `path: PathBuf` field to `Pager` and wrote `.open(path)?` followed by `path.as_ref()` — E0382, lib did not compile
- **Fix:** Hoist `let path_buf = path.as_ref().to_path_buf();`, open `&path_buf`, store `path_buf`
- **Files modified:** src/storage/pager.rs (2 lines)

**2. [Rule 1 - Bug] Replaced `unwrap_or*` with `match` in new code**
- **Found during:** Task 1 self-check (plan requires zero unwrap/expect)
- **Issue:** Three `unwrap_or*` call sites in new blink.rs code
- **Fix:** `match` expressions; remaining `unwrap_or*` in file belong to a parallel wave's `vacuum` methods — left untouched
- **Files modified:** src/index/blink.rs

## Verification

- `cargo test --test blink_tests` — 3 passed (split_grows_height, merge_preserves_order, scan_newest_first_matches_search)
- `cargo test --test concurrency_tests` — 1 passed (150-ops + snapshot + persistence)
- `cargo test --lib` — blocked by pre-existing E0502 borrow error in `src/storage/wal.rs:302` (parallel wave's file, out of scope — see Deferred)
- Acceptance: move-right loop at every level (`move_right` called per level in `descend` + search/scan chase); sibling-before-orig flush order in `split_leaf`/`split_interior`; `scan_all` newest-first via `fold_records` (rev-order, first-visible-wins, matches `search` line-79 semantics); oversize single-record probe returns `RowTooLarge(len)` (also covered by `pager_oversize_*` lib tests)

## Known Stubs

None in this plan's scope. `try_coalesce` is intentionally best-effort (errors ignored by caller) — full parent-separator cleanup is a Wave-3 upgrade; no keys are lost (right_link chain preserved, verified by merge test).

## Threat Flags

None beyond plan's register. T-01-03 mitigated: `MAX_RIGHT_HOPS` (1024) cap on descend/search/scan/vacuum chases + depth cap (64) → `CorruptPage`, no infinite chase. T-01-SC satisfied: no new deps.

## Deferred (out-of-scope, parallel-wave files)

- `src/storage/wal.rs:302` E0502 (`bytes[bytes.len()/2]` borrow) breaks `cargo test --lib` — belongs to the WAL wave; do not fix here. Re-run lib tests after that wave lands.
- `concurrency_tests` intermittently fails with Windows `PermissionDenied` when parallel waves run the same suite concurrently (shared `C:\tmp` pid/nanos temp names) — passes on retry; not caused by this plan.

## Self-Check: PASSED

- src/index/blink.rs contains `child_for`, `descend`, `split_leaf`, `split_interior`, `insert_separator`, `try_coalesce`, `fold_records`, `collect_leaves`; PageHeader lsn/high_key/right_link wired
- blink 3/3 + concurrency 1/1 green on final run
- No git repo in working directory — commits skipped per executor instructions
