# Codebase Concerns

**Analysis Date:** 2026-10-07

## Tech Debt

**B-Link tree interior navigation unimplemented (single-leaf only):**
- Issue: `find_leaf()` breaks out instead of descending interior nodes; tree never splits, all data accumulates in root leaf. Comment admits stub: "requires rigorous encoding which we skipped".
- Files: `src/index/blink.rs:50-70` (`break; // TODO: Implement interior search`), `src/storage/page.rs:32-35`
- Impact: Any table larger than one 64 KiB page silently misbehaves or overflows; no split/merge path. Blocks real B-tree growth.
- Fix approach: Define interior encoding (child PageIds as 8-byte LE in `NodeContent` or separate enum variant), implement descent + split with high_key/right_link updates, add multi-level test.

**Row encoding is ad-hoc delimited string:**
- Issue: Rows serialized as `val.join("|||")` with no escaping, no type info, no NULL handling; `SELECT` re-splits on same delimiter (`src/sql/executor.rs:90-94`, `src/sql/executor.rs:207`).
- Files: `src/sql/executor.rs:85-96`, `src/sql/executor.rs:203-211`
- Impact: Any `TEXT` value containing `|||` corrupts all columns; type fidelity lost (everything is string).
- Fix approach: Replace with length-prefixed tuple encoding (bincode/serde row struct) and decode by column type.

**UPDATE/DELETE only support `WHERE id = <literal>`:**
- Issue: `UPDATE`/`DELETE` pattern-match only `left.to_string() == "id"` with `Eq`; anything else silently affects 0 rows with a success message. `ALTER TABLE`/`DROP` return fake success without doing anything.
- Files: `src/sql/executor.rs:107-146`, `src/sql/executor.rs:147-174`, `src/sql/executor.rs:175-181`
- Impact: Silent no-ops mislead callers and UAT; full-table updates/deletes impossible.
- Fix approach: Return error/0-row honestly for unsupported predicates; implement general expression evaluation or explicit "unsupported" error.

**SELECT ignores WHERE, JOIN, ORDER BY, LIMIT, aggregates:**
- Issue: `execute_query()` reads table name only, calls `scan_all()`, returns every row unsorted/unfiltered.
- Files: `src/sql/executor.rs:185-217`
- Impact: Wrong results for any filtered/sorted query; forces client-side filtering.
- Fix approach: Evaluate `selection`, `order_by`, `limit` from `sqlparser` AST against decoded rows.

**Catalog is in-memory only, never persisted:**
- Issue: `Catalog { tables: HashMap }` holds the only mapping of table name → root page + `Arc<BLinkTree>`; nothing serializes it and server rebuilds empty `Catalog::new()` on every boot.
- Files: `src/catalog/mod.rs:28-38`, `src/bin/server.rs:13-14`
- Impact: Restart loses all tables even though page files remain on disk (writes to `titan_web.db` become orphaned). Reopen test in `src/bin/concurrency_test.rs:118-119` only re-reads raw page 0.
- Fix approach: Persist catalog (e.g., reserved catalog page / JSON sidecar) and rebuild `BLinkTree { root }` from stored root ids on open.

**No WAL, no fsync discipline, torn-write window:**
- Issue: `allocate_page()` → `flush_page()` writes a full 64 KiB buffer with a single `write_all` + `flush` (userspace only, no `sync_all`); `insert()`/`delete()` mutate page then flush synchronously on every op.
- Files: `src/storage/pager.rs:74-109`, `src/index/blink.rs:94-118`
- Impact: Crash between expiry-mark and insert leaves partial MVCC state; OS crash can tear pages. Every write pays a full-page I/O (no group commit).
- Fix approach: Add WAL with LSN (`PageHeader.lsn` in `src/storage/page.rs:27` is currently never set/incremented), `File::sync_all` on commit, background checkpoint.

**MVCC is timestamp-only with no transactions:**
- Issue: `GLOBAL_TX_ID` atomic hands out a fresh id per statement; `SELECT` even consumes a write id (`next_tx_id()` for `tx_read_ts`); no BEGIN/COMMIT/ROLLBACK, no conflict detection, no vacuum/GC of expired versions.
- Files: `src/sql/executor.rs:14`, `src/sql/executor.rs:26-28`, `src/sql/executor.rs:200`, `src/index/blink.rs:99-117`
- Impact: Unbounded version growth inside one leaf; snapshot semantics drift; expired records never reclaimed.
- Fix approach: Add transaction context, read-timestamp pinning, background vacuum that purges versions invisible to all active snapshots.

**`scan_all()` visibility logic is wrong for multi-version keys:**
- Issue: Forward iteration with `map.insert(key, Some)` on alive / conditional clear on dead mishandles interleaved versions (an old alive version after a newer delete resurrects the key).
- Files: `src/index/blink.rs:143-174`
- Impact: Deleted/updated rows can reappear depending on record order.
- Fix approach: Iterate per-key in reverse (newest-first, like `search()` at `src/index/blink.rs:79`) and take the first version visible at `tx_read_ts`.

**Page serialization silently truncates oversized pages:**
- Issue: `Page::serialize()` resizes up to `PAGE_SIZE` but never errors if `encoded.len() > PAGE_SIZE` — bincode output is then written past page boundary, corrupting the next page.
- Files: `src/storage/page.rs:67-77`, `src/storage/pager.rs:96-100`
- Impact: Large inserts corrupt neighbors; silent data loss.
- Fix approach: Return `TitanError` on oversize and trigger split/overflow pages (`PageType::Overflow` exists but is never used).

**Hardcoded 64 KiB page size with full-page read/write per op:**
- Issue: `PAGE_SIZE = 65536`, every `fetch_page` allocates a 64 KiB vec and `read_exact`s; every insert flushes a full page.
- Files: `src/storage/page.rs:4`, `src/storage/pager.rs:62`, `src/storage/pager.rs:95-105`
- Impact: Small rows cost 64 KiB I/O; write amplification is extreme; latency scales with page size not row size.
- Fix approach: Reduce default page size (8/16 KiB), add buffered/group-commit flush, measure with criterion benchmarks.

## Known Bugs

**`fetch_page` on a never-written offset errors instead of returning empty:**
- Symptoms: Opening a fresh DB and fetching any page except via `allocate_page` fails with `UnexpectedEof` from `read_exact`.
- Files: `src/storage/pager.rs:52-72`
- Trigger: `Pager::open()` on new file then `fetch_page(0)` before allocation.
- Workaround: Always `allocate_page()` first.

**`find_leaf` move-right can panic on malformed link:**
- Symptoms: `.expect("High key exists but no right link")` panics the thread (and poisons `std::sync::Mutex`es in the tree).
- Files: `src/index/blink.rs:38`
- Trigger: Corrupt or half-split page with `high_key.is_some()` and `right_link.is_none()`.
- Workaround: None — replace `expect` with a proper `TitanError`.

**Lock poisoning via `std::sync::Mutex + unwrap`:**
- Symptoms: Any panic while holding `root` or `file` poisons the mutex; subsequent `.unwrap()` panics cascade.
- Files: `src/index/blink.rs:23`, `src/index/blink.rs:29`, `src/index/blink.rs:144`
- Trigger: Concurrent test panic (see above).
- Workaround: Switch `BLinkTree.root` to `parking_lot::Mutex` (already a dependency) and map poison to `TitanError::LockError` as `src/storage/pager.rs:59` does.

**TOCTOU in `fetch_page`:**
- Symptoms: Two threads missing the cache concurrently both read the file and insert duplicate `Arc`s; one copy's writes can be lost.
- Files: `src/storage/pager.rs:52-72`
- Trigger: Concurrent first-touch of the same page id (see `src/bin/concurrency_test.rs`, `src/bin/ws_concurrency_test.rs`).
- Workaround: Hold shard write lock during fill, or double-check after acquiring write lock.

**`std::sync::Mutex<File>` serializes all I/O:**
- Symptoms: Throughput collapses under concurrent readers; file lock held across `read_exact`/`write_all`.
- Files: `src/storage/pager.rs:18`, `src/storage/pager.rs:59`, `src/storage/pager.rs:102`
- Trigger: Any multi-client workload via `src/bin/server.rs`.
- Workaround: Use `pread`/`pwrite` (positional I/O) or a lock-free file handle per thread.

## Security Considerations

**No authentication on HTTP/WS surface:**
- Risk: `warp::serve` binds `127.0.0.1:3030` with open `warp::fs::dir("web")` + `/ws` executing arbitrary SQL strings from any local client; no auth token, no TLS.
- Files: `src/bin/server.rs:20-33`, `src/bin/server.rs:36-63`
- Current mitigation: Localhost bind only.
- Recommendations: Add token auth filter, bind address config, TLS termination before any non-local exposure.

**Unbounded WebSocket input / SQL parsing DoS:**
- Risk: `executor.execute(text)` parses untrusted strings with `sqlparser` with no size cap; large/ deeply-nested payloads consume CPU per message in the async handler.
- Files: `src/bin/server.rs:48-55`, `src/sql/executor.rs:30-40`
- Current mitigation: None.
- Recommendations: Enforce max message size (`warp::ws` + content-length filter), timeout, and rate limit per connection.

**Path-adjacent risks (existence only):**
- Risk: Persistent file `titan_web.db` is created relative to CWD (`src/bin/server.rs:13`) — running from an unexpected directory scatters DB files; no secret files observed in repo.
- Files: `src/bin/server.rs:13`
- Current mitigation: Not applicable.
- Recommendations: Make DB path a CLI arg/env var; document backup story.

**Error strings leak internals:**
- Risk: Raw `format!("Error: {}", e)` including `io::Error` details is sent to every WS client.
- Files: `src/bin/server.rs:54`
- Current mitigation: None.
- Recommendations: Map to user-safe error codes; log details server-side only.

## Performance Bottlenecks

**Full-table `scan_all` on every SELECT:**
- Problem: `SELECT` materializes all `(key, value)` pairs into a `HashMap` then a `Vec`, decoding every row as UTF-8 + `split("|||")`.
- Files: `src/sql/executor.rs:201`, `src/index/blink.rs:143-174`
- Cause: No predicate pushdown, no index lookup for point queries, O(n) per query plus clone-heavy grouping (`rec.key.clone()`, `rec.data.clone()`).
- Improvement path: Route `WHERE id = ?` to `BLinkTree::search()`; stream rows instead of collecting.

**Synchronous flush per mutation:**
- Problem: `insert()`/`delete()` call `flush_page()` before returning; each flush takes shard read lock → page write lock → global file lock.
- Files: `src/index/blink.rs:115-116`, `src/index/blink.rs:137-138`, `src/storage/pager.rs:90-109`
- Cause: No write buffer, no batching.
- Improvement path: Dirty-page background flusher + commit-time `sync_all`.

**Linear scans where indexes expected:**
- Problem: `search()` reverse-scans all records in leaf; `find_leaf` linear-scan stub; `scan_all` groups via hashing all versions.
- Files: `src/index/blink.rs:73-92`, `src/index/blink.rs:143-174`
- Cause: Records stored as unsorted `Vec<MvccRecord>` (`src/storage/page.rs:34`).
- Improvement path: Keep records sorted by (key, tx_created), binary search + version chain.

**Excessive cloning on hot paths:**
- Problem: `page.clone()`, `header.clone()`, `content.clone()` on every serialize; key/data cloned on insert, search, and scan.
- Files: `src/storage/pager.rs:55`, `src/storage/page.rs:69-70`, `src/index/blink.rs:87`, `src/index/blink.rs:157`
- Cause: `Clone`-heavy owned-byte design.
- Improvement path: Borrow where possible, use `Bytes`/`Arc<[u8]>` for keys/values.

## Fragile Areas

**Pager shard + file lock ordering:**
- Files: `src/storage/pager.rs:48-109`
- Why fragile: Three lock types (`shard RwLock`, page `RwLock`, global `File Mutex`) acquired in different orders across `fetch/allocate/flush`; future code can deadlock by inverting order.
- Safe modification: Always acquire shard → page → file, never hold file lock while acquiring shard; add a lock-order comment and a multithreaded deadlock test.
- Test coverage: Only ad-hoc binaries (`src/bin/concurrency_test.rs`, `src/bin/ws_concurrency_test.rs`); no `cargo test` suite.

**B-Link split/expansion point:**
- Files: `src/index/blink.rs:26-71`
- Why fragile: Any change adding interior pages must maintain `high_key`/`right_link` invariants under concurrency; current single-leaf tests will not catch link bugs.
- Safe modification: Write split tests first (fill leaf, assert right-link chain), then implement.
- Test coverage: None — no unit tests in repo.

**SQL lowering in executor:**
- Files: `src/sql/executor.rs:42-183`
- Why fragile: Stringly-typed `to_string()` matching on `Expr`/`TableFactor` breaks silently on qualified names (`db.table`), aliases, quoted identifiers, or multi-row syntax variants.
- Safe modification: Match on AST variants structurally; add golden tests per statement type.
- Test coverage: None.

**Serialization format stability:**
- Files: `src/storage/page.rs:67-86`
- Why fragile: `bincode 1.3` with derived `Serialize` has no version field; any struct change invalidates existing `.db` files; trailing-zero padding makes `deserialize` sensitive to exact layout.
- Safe modification: Add format version byte + migration path before changing structs.
- Test coverage: Round-trip test missing.

## Scaling Limits

**Single-leaf capacity:**
- Current capacity: One 64 KiB page minus bincode overhead shared by all versions of all rows.
- Limit: Insert-heavy tables hit oversize/corruption (see serialize truncation) at low-thousands of rows.
- Scaling path: Implement splits + overflow pages.

**File size / page-id arithmetic:**
- Current capacity: `PageId = u64`, `total_pages = len / PAGE_SIZE` drops remainder bytes (`src/storage/pager.rs:31-32`).
- Limit: Partial trailing page silently ignored; 64 KiB granularity wastes space for tiny DBs.
- Scaling path: Handle remainder, allocate-on-demand, sparse file support.

**Concurrency ceiling:**
- Current capacity: Global file mutex + per-op flush caps useful concurrency at a handful of clients.
- Limit: WebSocket fan-out tests (`src/bin/ws_concurrency_test.rs`) will serialize on the file lock.
- Scaling path: Positional I/O, MVCC snapshot reads without file lock on cache hit, group commit.

## Dependencies at Risk

**`bincode 1.3` (legacy):**
- Risk: Old major; ecosystem moving to `bincode 2`; security/style lints differ.
- Impact: Serialization compat, future upgrade churn.
- Migration plan: Pin deliberate upgrade with version-gated migration test.

**`warp 0.3` + `tokio-tungstenite 0.21`:**
- Risk: `warp 0.3` aging; mixing two WS stacks (warp WS server + tungstenite test clients) complicates upgrades.
- Impact: Upgrade requires coordinated bump.
- Migration plan: Consolidate on one stack (e.g., `axum` + `tokio-tungstenite`) when touching the server.

**`sqlparser 0.43`:**
- Risk: Fast-moving API; AST shapes change across minors.
- Impact: Executor's fragile string matching breaks on upgrade.
- Migration plan: Lock version, add parser golden tests before bumping.

## Missing Critical Features

**Persistence across restarts:**
- Problem: Catalog loss (above) means the DB is effectively ephemeral across reboots.
- Blocks: Any production use; every deploy wipes schema.

**Durability/ACID:**
- Problem: No WAL, no commit protocol, no recovery replay.
- Blocks: Trusting the DB with non-reproducible data.

**Test suite:**
- Problem: Zero `#[test]`s; only manual binaries in `src/bin/` (`concurrency_test.rs`, `ws_concurrency_test.rs`, `dataset_populator.rs`, `server.rs`).
- Blocks: Safe refactoring of pager/tree/executor.

## Test Coverage Gaps

**Untested: pager crash consistency:**
- What's not tested: Torn writes, partial flushes, reopen recovery.
- Files: `src/storage/pager.rs`, `src/storage/page.rs`
- Risk: Silent corruption after crash goes unnoticed.
- Priority: High

**Untested: MVCC visibility:**
- What's not tested: Multi-version insert/update/delete/read-snapshot matrix, `scan_all` vs `search` agreement.
- Files: `src/index/blink.rs:73-174`
- Risk: Resurrected/deleted-row bugs ship silently.
- Priority: High

**Untested: SQL semantics:**
- What's not tested: All statement types, unsupported-syntax errors, type coercion, `|||`-in-text corruption case.
- Files: `src/sql/executor.rs`
- Risk: Wrong results and fake-success DDL.
- Priority: High

**Untested: concurrent access:**
- What's not tested: No automated concurrency/durability assertions; binaries require a live server and eyeballing.
- Files: `src/bin/concurrency_test.rs`, `src/bin/ws_concurrency_test.rs`
- Risk: Data races, lost updates, deadlocks regress freely.
- Priority: Medium

---

*Concerns audit: 2026-10-07*
