//! D-04 contracts: MVCC snapshot isolation matrix (TxManager edition).

use std::sync::Arc;
use parking_lot::RwLock;
use titan_db::catalog::Catalog;
use titan_db::sql::executor::Executor;
use titan_db::sql::ExecutionResult;
use titan_db::storage::pager::Pager;
use titan_db::txn::TxManager;

fn setup(name: &str) -> (Arc<Pager>, Arc<RwLock<Catalog>>, Executor, std::path::PathBuf) {
    let base = std::env::var("TITAN_TEST_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let path = std::path::PathBuf::from(base).join(format!("titan_mvcc_{}_{}.db", name, std::process::id()));
    let _ = std::fs::remove_file(&path);
    let pager = Arc::new(Pager::open(&path).expect("open"));
    let catalog = Arc::new(RwLock::new(Catalog::new()));
    let exec = Executor::new(pager.clone(), catalog.clone());
    (pager, catalog, exec, path)
}

fn count_rows(exec: &Executor) -> usize {
    match exec.execute("SELECT * FROM m").expect("select") {
        ExecutionResult::ResultSet { rows, .. } => rows.len(),
        ExecutionResult::Message(m) => panic!("expected ResultSet, got: {}", m),
    }
}

#[test]
fn mvcc_no_dirty_reads() {
    let (_p, _c, exec, _path) = setup("dirty");
    exec.execute("CREATE TABLE m (id INT, name TEXT)").expect("create");
    exec.execute("INSERT INTO m (id, name) VALUES (1, 'one')").expect("insert");
    let snap = exec.next_tx_id();
    // Later write happens after snapshot was pinned.
    exec.execute("INSERT INTO m (id, name) VALUES (2, 'two')").expect("insert2");
    // Snapshot read must NOT see row 2. Requires read-ts pinning.
    let cat = _c.read();
    let schema = cat.tables.get("m").expect("table");
    let rows = schema.tree.scan_all(snap).expect("scan at snapshot");
    assert_eq!(rows.len(), 1, "dirty read: snapshot saw post-snapshot write");
}

#[test]
fn mvcc_first_committer_wins() {
    // Two concurrent txns write the same key; exactly one must commit,
    // the loser gets TxConflict. Exercised at the TxManager level.
    let m = TxManager::new();
    let t1 = m.begin();
    let t2 = m.begin();
    m.record_write(t1.tx_id, b"1".to_vec());
    m.record_write(t2.tx_id, b"1".to_vec());
    assert!(m.commit(t1).is_ok(), "first committer must succeed");
    assert!(
        matches!(m.commit(t2), Err(titan_db::TitanError::TxConflict)),
        "second committer to same key must get TxConflict (lost update)"
    );
}

#[test]
fn mvcc_rollback_invisible() {
    let (_p, _c, exec, _path) = setup("rollback");
    exec.execute("CREATE TABLE m (id INT, name TEXT)").expect("create");
    exec.execute("INSERT INTO m (id, name) VALUES (1, 'keep')").expect("insert");
    exec.execute("BEGIN").expect("begin");
    exec.execute("INSERT INTO m (id, name) VALUES (2, 'phantom')").expect("buffered insert");
    // Own writes visible inside the txn...
    assert_eq!(count_rows(&exec), 2, "own buffered write must be visible in-txn");
    exec.execute("ROLLBACK").expect("rollback");
    // ...but invisible after rollback.
    let n = count_rows(&exec);
    assert_eq!(n, 1, "rollback left phantom rows");
}

#[test]
fn mvcc_begin_commit_roundtrip() {
    let (_p, _c, exec, _path) = setup("commit");
    exec.execute("CREATE TABLE m (id INT, name TEXT)").expect("create");
    exec.execute("BEGIN").expect("begin");
    exec.execute("INSERT INTO m (id, name) VALUES (7, 'seven')").expect("insert");
    exec.execute("COMMIT").expect("commit");
    assert_eq!(count_rows(&exec), 1);
}

#[test]
fn mvcc_vacuum_reclaims_only_below_horizon() {
    let (_p, _c, exec, _path) = setup("vacuum");
    exec.execute("CREATE TABLE m (id INT, name TEXT)").expect("create");
    for i in 0..5 {
        exec.execute(&format!("INSERT INTO m (id, name) VALUES (1, 'v{}')", i)).expect("insert");
    }
    // No active txns -> horizon is u64::MAX -> vacuum may reclaim old versions.
    let reclaimed = exec.vacuum().expect("vacuum");
    assert!(reclaimed >= 4, "vacuum must reclaim superseded versions, got {}", reclaimed);
    // Latest version survives.
    assert_eq!(count_rows(&exec), 1);
}
