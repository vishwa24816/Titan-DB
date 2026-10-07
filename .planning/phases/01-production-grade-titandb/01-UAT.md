---
status: testing
phase: 01-production-grade-titandb
source: 01-01-SUMMARY.md, 01-02-SUMMARY.md, 01-03-SUMMARY.md, 01-04-SUMMARY.md, 01-05-SUMMARY.md, 01-06-SUMMARY.md, 01-07-SUMMARY.md
started: 2026-10-07T16:16:38Z
updated: 2026-10-07T16:16:38Z
---

## Current Test

number: 1
name: Cold Start Smoke Test
expected: |
  Kill any running server. Delete *.db files. Run `cargo run` — engine boots without errors, CREATE/INSERT/SELECT demo prints results.
awaiting: user response

## Tests

### 1. Cold Start Smoke Test
expected: Kill server, delete *.db, `cargo run` — boots clean, demo CREATE/INSERT/SELECT prints results
result: [pending]

### 2. Full SQL Workflow
expected: Via CLI or server — CREATE TABLE, multi-row INSERT, SELECT with WHERE/ORDER BY/LIMIT, JOIN, aggregates, window functions all return correct rows
result: [pending]

### 3. Transactions + MVCC Snapshot
expected: BEGIN, INSERT inside tx invisible to others until COMMIT; ROLLBACK discards; concurrent readers see stable snapshot
result: [pending]

### 4. Durability Across Restart
expected: INSERT rows, kill process, restart — rows still there (WAL recovery + persisted catalog)
result: [pending]

### 5. Error Handling (honest errors)
expected: Bad SQL / missing table / unsupported syntax returns clean user-safe error message, no panic, no fake success
result: [pending]

### 6. Web UI + Transaction Manager
expected: `cargo run --bin server`, open http://localhost:3030 — run query, see result grid; Begin/Commit/Rollback buttons work
result: [pending]

### 7. Concurrency Stress
expected: `cargo test` full suite green (31 tests) — run it and confirm 0 failures
result: [pending]

## Summary

total: 7
passed: 0
issues: 0
pending: 7
skipped: 0
blocked: 0

## Gaps

[none yet]
