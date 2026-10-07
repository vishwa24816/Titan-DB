//! D-06 fail-first contracts: WAL durability, atomicity, corruption detection.
//! These tests SHOULD FAIL until ARIES-lite WAL + recovery lands.

use std::sync::Arc;
use parking_lot::RwLock;
use titan_db::catalog::Catalog;
use titan_db::sql::executor::Executor;
use titan_db::sql::ExecutionResult;
use titan_db::storage::pager::Pager;

fn test_dir() -> std::path::PathBuf {
    let base = std::env::var("TITAN_TEST_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let dir = std::path::PathBuf::from(base).join(format!("titan_wal_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn unique_db(name: &str) -> std::path::PathBuf {
    let p = test_dir().join(format!("{}_{}.db", name, std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).expect("time").as_nanos()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn wal_commit_durable() {
    let path = unique_db("commit");
    {
        let pager = Arc::new(Pager::open(&path).expect("open"));
        let catalog = Arc::new(RwLock::new(Catalog::new()));
        let exec = Executor::new(pager.clone(), catalog.clone());
        exec.execute("CREATE TABLE t (id INT, name TEXT)").expect("create");
        exec.execute("INSERT INTO t (id, name) VALUES (1, 'durable')").expect("insert");
        // Commit path must sync_all; drop everything to simulate clean shutdown.
    }
    // Reopen: committed data must be present (requires persisted catalog + WAL replay).
    let pager = Arc::new(Pager::open(&path).expect("reopen"));
    let catalog = Arc::new(RwLock::new(Catalog::new()));
    let exec = Executor::new(pager.clone(), catalog.clone());
    let res = exec.execute("SELECT * FROM t").expect("select after reopen");
    match res {
        ExecutionResult::ResultSet { rows, .. } => {
            assert!(!rows.is_empty(), "committed row lost after reopen — WAL replay missing");
        }
        ExecutionResult::Message(m) => panic!("expected ResultSet after reopen, got Message: {}", m),
    }
}

#[test]
fn wal_uncommitted_gone() {
    // Crash-before-commit: uncommitted writes must NOT be visible after recovery.
    // Current engine has no tx boundaries (every stmt auto-commits), so this
    // fails until BEGIN/COMMIT with WAL commit records exists.
    let path = unique_db("uncommitted");
    {
        let pager = Arc::new(Pager::open(&path).expect("open"));
        let catalog = Arc::new(RwLock::new(Catalog::new()));
        let exec = Executor::new(pager.clone(), catalog.clone());
        exec.execute("CREATE TABLE u (id INT, name TEXT)").expect("create");
        exec.execute("INSERT INTO u (id, name) VALUES (1, 'committed')").expect("insert");
        // Simulate an uncommitted txn: there is no COMMIT api yet; a real
        // BEGIN..INSERT (no COMMIT) + crash must leave only row 1.
        // Until then, assert the boundary exists by checking BEGIN is honored.
        let begin = exec.execute("BEGIN");
        assert!(begin.is_ok(), "BEGIN must be supported for atomicity");
    }
    let pager = Arc::new(Pager::open(&path).expect("reopen"));
    let catalog = Arc::new(RwLock::new(Catalog::new()));
    let exec = Executor::new(pager, catalog);
    let res = exec.execute("SELECT * FROM u").expect("select");
    match res {
        ExecutionResult::ResultSet { rows, .. } => {
            assert_eq!(rows.len(), 1, "uncommitted data leaked across crash");
        }
        ExecutionResult::Message(m) => panic!("expected ResultSet, got: {}", m),
    }
}

#[test]
fn wal_corrupt_frame_errors() {
    let path = unique_db("corrupt");
    {
        let pager = Arc::new(Pager::open(&path).expect("open"));
        let catalog = Arc::new(RwLock::new(Catalog::new()));
        let exec = Executor::new(pager, catalog);
        exec.execute("CREATE TABLE c (id INT, name TEXT)").expect("create");
        exec.execute("INSERT INTO c (id, name) VALUES (7, 'seven')").expect("insert");
    }
    // Flip a byte mid-file to simulate a torn/corrupt WAL frame.
    let bytes = std::fs::read(&path).expect("read db");
    assert!(bytes.len() > 64, "db file too small to corrupt");
    let mut bad = bytes.clone();
    let off = bytes.len() / 2;
    bad[off] ^= 0xFF;
    std::fs::write(&path, &bad).expect("write corrupt");
    // Recovery must return a corruption error — never panic.
    let result = std::panic::catch_unwind(|| {
        let pager = Pager::open(&path);
        match pager {
            Ok(p) => {
                // Any page access on corrupt data must error, not return garbage.
                let r = p.fetch_page(0);
                assert!(r.is_err(), "corrupt frame accepted without CorruptPage/WalCorrupt error");
            }
            Err(_) => { /* open-time detection is also acceptable */ }
        }
    });
    assert!(result.is_ok(), "recovery PANICKED on corrupt frame instead of returning an error");
}
