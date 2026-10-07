---
status: resolved
slug: io-error-frontend
trigger: still it shows IO error, solve the issue at front end, api or websocket level
created: 2026-10-07
updated: 2026-10-07
---

## Symptoms

- expected: Example queries from p.md run in the web UI and show results or clear messages
- actual: UI shows IO error (status error, code IO_ERROR, message "query failed")
- errors: `{"status":"error","code":"IO_ERROR","error":"query failed"}` — misleading code, real message swallowed
- timeline: Since phase-1 server (enveloped WS protocol with user_safe_code)
- reproduction: Run `CREATE TABLE users ...` when table already exists (DB still holds prior test rows) — first response is IO_ERROR. Also any SQL parse failure maps to IO_ERROR.

## Evidence

- timestamp: 2026-10-07 — python WS repro: CREATE TABLE on existing table -> IO_ERROR/"query failed"; all other CRUD ok. So error path itself works, but code+message are wrong/misleading.
- hypothesis: executor maps sqlparser errors and "table exists" into TitanError::Io, and handle_op replaces the message with generic "query failed"
- next_action: trace user_safe_code() + handle_op error mapping, fix at API/WS level

## Current Focus

hypothesis: CONFIRMED — executor mapped parse errors, table-exists, and table-not-found into TitanError::Io, and server.rs replaced the message with generic "query failed"
next_action: done — fixed and verified over real WS

## Resolution

root_cause: executor.rs wrapped SQL parse failures, "table already exists", and "table not found" as TitanError::Io, and server.rs discarded the inner message, sending code IO_ERROR + "query failed"
fix: added TitanError::InvalidSql/TableExists/TableNotFound variants with codes INVALID_SQL/TABLE_EXISTS/TABLE_NOT_FOUND + user_message(); server.rs now sends the user-safe message; frontend (web/index.html) already renders code + message, no change needed
verification: rebuilt, restarted detached server, real WS (ws_verify_err.py) — CREATE TABLE existing -> TABLE_EXISTS/"Table users already exists"; bad SQL -> INVALID_SQL + parser detail; missing table SELECT/INSERT -> TABLE_NOT_FOUND; normal SELECT still ok
