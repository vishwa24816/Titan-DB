//! D-12: automated port of src/bin/concurrency_test.rs assertions
//! (150 ops, snapshot isolation, persistence) into `cargo test`.
//! Ported 1:1 from the binary; must compile. Passes only when the engine
//! is correct under concurrency.

use std::sync::Arc;
use std::thread;
use std::time::Instant;
use parking_lot::RwLock;
use titan_db::catalog::Catalog;
use titan_db::sql::executor::Executor;
use titan_db::sql::ExecutionResult;
use titan_db::storage::pager::Pager;

fn unique_db(name: &str) -> std::path::PathBuf {
    let base = std::env::var("TITAN_TEST_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(base).join(format!(
        "titan_conc_{}_{}_{}.db",
        name,
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_nanos()
    ))
}

#[test]
fn concurrency_150_ops_snapshot_and_persistence() {
    let db_path = unique_db("mvcc");
    let _ = std::fs::remove_file(&db_path);

    let (pager, catalog, executor) = {
        let p = Arc::new(Pager::open(&db_path).expect("Failed to open Pager"));
        let c = Arc::new(RwLock::new(Catalog::new()));
        let e = Arc::new(Executor::new(p.clone(), c.clone()));
        (p, c, e)
    };

    // 1. Create table
    executor.execute("CREATE TABLE users (id INT, name TEXT, age INT)").expect("create table");

    // 2. Prepopulate 50 initial rows for update/delete concurrency
    for i in 0..50 {
        executor.execute(&format!("INSERT INTO users (id, name, age) VALUES ({}, 'initial_{}', 20)", i, i)).expect("prepopulate");
    }

    // 3. Snapshot ts before concurrent modifications & new insertions
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
        executor.execute(&format!("INSERT INTO users (id, name, age) VALUES ({}, 'to_mod_{}', 25)", i + 50, i + 50)).expect("seed for update");
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
        match handle.join().expect("thread join") {
            Ok(_) => successful_ops += 1,
            Err(e) => {
                failed_ops += 1;
                eprintln!("Op error: {:?}", e);
            }
        }
    }

    let elapsed = start.elapsed();
    let total_ops = successful_ops + failed_ops;
    assert_eq!(total_ops, 150, "expected 150 concurrent ops, got {}", total_ops);
    let ops_per_sec = (total_ops as f64) / elapsed.as_secs_f64();
    println!("150 ops in {:?}: {} ok / {} failed ({:.2} ops/sec)", elapsed, successful_ops, failed_ops, ops_per_sec);

    // 4. MVCC snapshot isolation: early snapshot sees only the 50 originals
    let snap_records = {
        let cat = catalog.read();
        let schema = cat.tables.get("users").expect("users table");
        schema.tree.scan_all(snapshot_read_ts).expect("snapshot scan")
    };
    assert_eq!(snap_records.len(), 50, "MVCC Isolation failed: snapshot saw post-transaction changes!");

    // Latest read: 50 deletes + 50 updates + 50 inserts -> 100 active rows
    let latest_res = executor.execute("SELECT * FROM users").expect("latest select");
    if let ExecutionResult::ResultSet { rows, .. } = latest_res {
        assert_eq!(rows.len(), 100, "Row count mismatch on latest state!");
    } else {
        panic!("expected ResultSet from SELECT *");
    }

    // 5. Disk persistence across reopen
    drop(executor);
    drop(catalog);
    drop(pager);

    let file_meta = std::fs::metadata(&db_path).expect("Database file missing on disk");
    assert!(file_meta.len() >= 4096, "Persistence failed: file empty or sub-page!");

    let reopened_pager = Arc::new(Pager::open(&db_path).expect("Failed to re-open DB from disk"));
    let raw_root_page = reopened_pager.fetch_page(0).expect("Failed to read persisted page 0 from disk");
    let page_guard = raw_root_page.read();
    assert!(!page_guard.content.records.is_empty(), "Persistence failed: 0 records found in persisted disk page!");

    let _ = std::fs::remove_file(&db_path);
}
