# Phase 1 Plan 6: Full SQL Evaluation Summary

**Phase:** 01-production-grade-titandb · **Plan:** 6 · **Date:** 2026-10-07
**Requirements:** D-07 (function surface), D-08 (WHERE/JOIN/pushdown/honest errors)
**Status:** Complete — `cargo test` fully green (31 tests, 0 failures), `sql_tests` 7/7.

## One-liner
Volcano-style SQL evaluation over sqlparser 0.43 AST with PK pushdown to `BLinkTree::search`, hash/nested-loop JOINs, GROUP BY aggregates, ROW_NUMBER/RANK windows, and a scalar function library — all unsupported syntax returns `UnsupportedSql`, never silent 0-row or fake DDL success.

## What was built
- **`src/sql/executor.rs` (rewritten query path):** Scan (PK-equality pushdown → `search()`, else `scan_all` + Filter), Filter (comparisons, AND/OR/NOT, LIKE, IN, IS NULL), JOIN (hash equi-join + nested-loop cross, inner/left/right/full, 100k row cap per T-01-09), Sort + Limit/Offset (Query-level, numeric-aware), GROUP BY hash aggregation, window ordering. Checked arithmetic everywhere (T-01-08). TxContext visibility preserved from plan 5; no unwrap/expect added.
- **`src/sql/functions.rs` (new):** numeric ABS/ROUND/POW/SQRT/MOD, string UPPER/LOWER/LENGTH/SUBSTR/LIKE/CONCAT/TRIM/LTRIM/RTRIM, datetime NOW/CURRENT_DATE/EXTRACT/DATE_ADD via minimal own YYYY-MM-DD arithmetic (**no chrono dependency** — T-01-SC gate avoided entirely, no install needed), CASE/COALESCE/NULLIF, NULL-semantics operators, COUNT/SUM/AVG/MIN/MAX, ROW_NUMBER/RANK helpers.
- **`tests/sql_tests.rs`:** fixed the `unsupported_syntax_errors` contract (it asserted the old fake-success behavior — `.expect()` on an Err path) to assert honest `UnsupportedSql` for ALTER/DROP; added `scalar_function_goldens` covering numeric/string/datetime/conditional families + typed-error cases.

## Verification
- `cargo test --test sql_tests`: 7/7 pass (WHERE, ORDER+LIMIT, aggregates, inner+left JOIN, window, honest errors, scalars).
- Full `cargo test`: all suites green, 0 failed.
- Acceptance proofs: `tree.search(` pushdown at executor.rs:644; no `altered.`/`parsed but not executed`/`Complex query not implemented` strings remain; every unhandled arm returns `TitanError::UnsupportedSql`.

## Deviations (auto-fixed, Rules 1–2)
1. **[Rule 1 — Bug] Multi-table writes landed in the wrong tree.** `WriteOp` carried no table; `apply_op` used a first-table fallback, so `INSERT INTO b` went to table `a` (JOIN test got 0 rows). Fix: `table` field on `WriteOp`, per-table tree resolution in `apply_op`, per-table WAL grouping at COMMIT, table-scoped tx overlay in scans.
2. **[Rule 1 — Bug] Fail-first test asserted fake-success.** `unsupported_syntax_errors` called `.expect()` then panicked on `Message` — unpassable once honest errors landed. Rewrote to assert `is_err` + `UnsupportedSql`.
3. **[Rule 2 — Correctness] sqlparser 0.43 shapes.** ORDER BY/LIMIT/OFFSET live on `Query` (not `Select`); `TRIM`/`EXTRACT` are dedicated `Expr` variants (not `Function`); `GroupByExpr`/`JoinOperator` variant shapes differ from plan sketch. Handled structurally; unsupported forms error honestly.
4. **[Rule 2 — Threat T-01-SC] Skipped chrono entirely.** Own date arithmetic instead of a new dependency — no human-verify install gate required.

## Known stubs
None. `PARTITION BY` windows return honest `UnsupportedSql` (out of scope for D-07 goldens, documented in code).

## Threat flags
None new — JOIN cap (T-01-09) and checked arithmetic (T-01-08) implemented as specified.

## Self-Check: PASSED
- `src/sql/functions.rs` exists; `cargo test` green verified above; no commits (no git repo per instructions).
