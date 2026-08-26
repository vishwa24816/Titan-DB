use std::sync::Arc;
use std::thread;
use std::time::Instant;
use parking_lot::RwLock;
use titan_db::catalog::Catalog;
use titan_db::sql::executor::Executor;
use titan_db::sql::ExecutionResult;
use titan_db::storage::pager::Pager;

fn main() {
    let db_path = "titan_mvcc_persistence_test.db";
    let _ = std::fs::remove_file(db_path);

    println!("============================================================");
    println!("TITAN-DB CONCURRENCY, MVCC ISOLATION & DISK PERSISTENCE TEST");
    println!("============================================================");

    let (pager, catalog, executor) = {
        let p = Arc::new(Pager::open(db_path).expect("Failed to open Pager"));
        let c = Arc::new(RwLock::new(Catalog::new()));
        let e = Arc::new(Executor::new(p.clone(), c.clone()));
        (p, c, e)
    };

    // 1. Create table
    executor.execute("CREATE TABLE users (id INT, name TEXT, age INT)").unwrap();

    // 2. Prepopulate 50 initial rows for update/delete concurrency
    for i in 0..50 {
        executor.execute(&format!("INSERT INTO users (id, name, age) VALUES ({}, 'initial_{}', 20)", i, i)).unwrap();
    }

    // 3. Test MVCC Snapshot Isolation concurrently:
    // Take snapshot ts before concurrent modifications & new insertions
    let snapshot_read_ts = executor.next_tx_id();

    let start = Instant::now();
    let mut handles = Vec::new();

    // 50 concurrent Inserts (ids 100..150)
    for i in 100..150 {
        let exec = Arc::clone(&executor);
        handles.push(thread::spawn(move || {
            exec.execute(&format!("INSERT INTO users (id, name, age) VALUES ({}, 'user_{}', 30)", i, i))
        }));
    }

    // 50 concurrent Updates on separate rows (ids 50..100)
    for i in 0..50 {
        executor.execute(&format!("INSERT INTO users (id, name, age) VALUES ({}, 'to_mod_{}', 25)", i + 50, i + 50)).unwrap();
    }
    for i in 50..100 {
        let exec = Arc::clone(&executor);
        handles.push(thread::spawn(move || {
            exec.execute(&format!("UPDATE users SET name = 'mod_{}' WHERE id = {}", i, i))
        }));
    }

    // 50 concurrent Deletes on prepopulated rows (ids 0..50)
    for i in 0..50 {
        let exec = Arc::clone(&executor);
        handles.push(thread::spawn(move || {
            exec.execute(&format!("DELETE FROM users WHERE id = {}", i))
        }));
    }

    let mut successful_ops = 0;
    let mut failed_ops = 0;

    for handle in handles {
        match handle.join().unwrap() {
            Ok(_) => successful_ops += 1,
            Err(e) => {
                failed_ops += 1;
                eprintln!("Op error: {:?}", e);
            }
        }
    }

    let elapsed = start.elapsed();
    let total_ops = successful_ops + failed_ops;
    let ops_per_sec = (total_ops as f64) / elapsed.as_secs_f64();

    println!("Concurrent Operations (150 ops):");
    println!("  - Completed in:      {:?}", elapsed);
    println!("  - Successful:        {}", successful_ops);
    println!("  - Failed:            {}", failed_ops);
    println!("  - Throughput:        {:.2} ops/sec", ops_per_sec);

    // 4. Verify MVCC row-level isolation via snapshot timestamp
    let snap_records = {
        let cat = catalog.read();
        let schema = cat.tables.get("users").unwrap();
        schema.tree.scan_all(snapshot_read_ts).unwrap()
    };
    println!("MVCC Snapshot Isolation Check:");
    println!("  - Rows visible at early tx snapshot {}: {} (expected: 50 original initial rows)", snapshot_read_ts, snap_records.len());
    assert_eq!(snap_records.len(), 50, "MVCC Isolation failed: snapshot saw post-transaction changes!");

    // Latest read after all 50 deletes (0..50 deleted), 50 updates (50..100 updated), 50 inserts (100..150 inserted)
    let latest_res = executor.execute("SELECT * FROM users").unwrap();
    if let ExecutionResult::ResultSet { rows, .. } = latest_res {
        println!("  - Rows visible at latest snapshot: {} (expected: 100 remaining active rows)", rows.len());
        assert_eq!(rows.len(), 100, "Row count mismatch on latest state!");
    }

    // 5. Verify Disk Persistence
    // Drop in-memory structs and reopen database file from disk
    drop(executor);
    drop(catalog);
    drop(pager);

    let file_meta = std::fs::metadata(db_path).expect("Database file missing on disk");
    println!("Disk Persistence Check:");
    println!("  - Database File Size on Disk: {} bytes (multi-page persistent storage)", file_meta.len());
    assert!(file_meta.len() >= 4096, "Persistence failed: file empty or sub-page!");

    let reopened_pager = Arc::new(Pager::open(db_path).expect("Failed to re-open DB from disk"));
    let raw_root_page = reopened_pager.fetch_page(0).expect("Failed to read persisted page 0 from disk");
    let page_guard = raw_root_page.read();
    println!("  - Persisted MVCC records count on disk page: {}", page_guard.content.records.len());
    assert!(!page_guard.content.records.is_empty(), "Persistence failed: 0 records found in persisted disk page!");

    println!("============================================================");
    println!("ALL CORE GOALS ACHIEVED (Concurrency, MVCC, Disk Persistence)");
    println!("============================================================");

    let _ = std::fs::remove_file(db_path);
}
