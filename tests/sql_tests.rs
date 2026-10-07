//! D-07/D-08 fail-first contracts: SQL semantics goldens + honest errors.
//! Golden tests SHOULD FAIL until WHERE/ORDER/LIMIT/JOIN/aggregate/window
//! evaluation lands. Honest-error tests fail while fake DDL succeeds silently.

use std::sync::Arc;
use parking_lot::RwLock;
use titan_db::catalog::Catalog;
use titan_db::sql::executor::Executor;
use titan_db::sql::ExecutionResult;
use titan_db::storage::pager::Pager;

fn setup(name: &str) -> Executor {
    let base = std::env::var("TITAN_TEST_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let path = std::path::PathBuf::from(base).join(format!("titan_sql_{}_{}.db", name, std::process::id()));
    let _ = std::fs::remove_file(&path);
    let pager = Arc::new(Pager::open(&path).expect("open"));
    let catalog = Arc::new(RwLock::new(Catalog::new()));
    Executor::new(pager, catalog)
}

fn rows_of(exec: &Executor, sql: &str) -> Vec<Vec<String>> {
    match exec.execute(sql).expect("execute") {
        ExecutionResult::ResultSet { rows, .. } => rows,
        ExecutionResult::Message(m) => panic!("expected ResultSet for {:?}, got Message: {}", sql, m),
    }
}

fn seed_small(exec: &Executor) {
    exec.execute("CREATE TABLE t (id INT, name TEXT, age INT)").expect("create");
    for (id, name, age) in [(1, "ann", 30), (2, "bob", 25), (3, "cid", 35)] {
        exec.execute(&format!("INSERT INTO t (id, name, age) VALUES ({}, '{}', {})", id, name, age))
            .expect("insert");
    }
}

#[test]
fn sql_where_filters_rows() {
    let exec = setup("where");
    seed_small(&exec);
    let rows = rows_of(&exec, "SELECT * FROM t WHERE id = 2");
    assert_eq!(rows.len(), 1, "WHERE id = 2 must return exactly 1 row, got {:?}", rows);
}

#[test]
fn sql_order_limit_golden() {
    let exec = setup("orderlimit");
    seed_small(&exec);
    // ORDER BY age DESC LIMIT 2 -> cid(35), ann(30)
    let rows = rows_of(&exec, "SELECT * FROM t ORDER BY age DESC LIMIT 2");
    assert_eq!(rows.len(), 2, "ORDER BY..LIMIT must return 2 rows, got {:?}", rows);
    let ages: Vec<String> = rows.iter().map(|r| r[2].clone()).collect();
    assert_eq!(ages, vec!["35".to_string(), "30".to_string()], "ORDER BY age DESC wrong: {:?}", rows);
}

#[test]
fn sql_aggregate_golden() {
    let exec = setup("aggr");
    seed_small(&exec);
    let rows = rows_of(&exec, "SELECT COUNT(*), SUM(age), AVG(age), MIN(age), MAX(age) FROM t");
    assert_eq!(rows.len(), 1, "aggregate must return 1 row, got {:?}", rows);
    assert_eq!(rows[0], vec!["3", "90", "30", "25", "35"], "aggregate golden mismatch: {:?}", rows[0]);
}

#[test]
fn sql_join_golden() {
    let exec = setup("join");
    exec.execute("CREATE TABLE a (id INT, name TEXT)").expect("create a");
    exec.execute("CREATE TABLE b (id INT, name TEXT)").expect("create b");
    exec.execute("INSERT INTO a (id, name) VALUES (1, 'x')").expect("i1");
    exec.execute("INSERT INTO a (id, name) VALUES (2, 'y')").expect("i2");
    exec.execute("INSERT INTO b (id, name) VALUES (2, 'yy')").expect("i3");
    exec.execute("INSERT INTO b (id, name) VALUES (3, 'zz')").expect("i4");
    let inner = rows_of(&exec, "SELECT * FROM a INNER JOIN b ON a.id = b.id");
    assert_eq!(inner.len(), 1, "inner join golden: expected 1 row, got {:?}", inner);
    let left = rows_of(&exec, "SELECT * FROM a LEFT JOIN b ON a.id = b.id");
    assert_eq!(left.len(), 2, "left join golden: expected 2 rows, got {:?}", left);
}

#[test]
fn sql_window_golden() {
    let exec = setup("window");
    seed_small(&exec);
    let rows = rows_of(&exec, "SELECT id, ROW_NUMBER() OVER (ORDER BY age) FROM t");
    assert_eq!(rows.len(), 3, "window must return 3 rows, got {:?}", rows);
    // bob(25)->1, ann(30)->2, cid(35)->3
    let rn: Vec<String> = rows.iter().map(|r| r[r.len() - 1].clone()).collect();
    assert_eq!(rn, vec!["1", "2", "3"], "ROW_NUMBER golden mismatch: {:?}", rows);
}

#[test]
fn unsupported_syntax_errors() {
    let exec = setup("unsupported");
    exec.execute("CREATE TABLE t (id INT, name TEXT)").expect("create");
    // Fake-success DDL and silent 0-row paths must become honest errors.
    let alter = exec.execute("ALTER TABLE t ADD COLUMN extra INT");
    assert!(alter.is_err(), "ALTER TABLE must be an honest error, got {:?}", alter);
    assert!(
        format!("{:?}", alter.unwrap_err()).contains("UnsupportedSql"),
        "ALTER must be UnsupportedSql"
    );
    let drop = exec.execute("DROP TABLE t");
    assert!(drop.is_err(), "DROP must be an honest error, got {:?}", drop);
}

#[test]
fn scalar_function_goldens() {
    let exec = setup("scalarfn");
    exec.execute("CREATE TABLE t (id INT, name TEXT, age INT)").expect("create");
    exec.execute("INSERT INTO t (id, name, age) VALUES (1, 'ann', 30)").expect("i1");
    let rows = rows_of(&exec, "SELECT UPPER(name), LOWER(name), LENGTH(name), ABS(age), COALESCE(name, 'x') FROM t WHERE id = 1");
    assert_eq!(rows[0], vec!["ANN", "ann", "3", "30", "ann"], "scalar golden mismatch: {:?}", rows[0]);
    let rows = rows_of(&exec, "SELECT MOD(age, 7), ROUND(age), CONCAT(name, '!'), TRIM(name) FROM t WHERE id = 1");
    assert_eq!(rows[0], vec!["2", "30", "ann!", "ann"], "numeric/string golden mismatch: {:?}", rows[0]);
    let rows = rows_of(&exec, "SELECT CURRENT_DATE, NOW(), EXTRACT(YEAR FROM CURRENT_DATE), DATE_ADD(CURRENT_DATE, 1) FROM t WHERE id = 1");
    assert_eq!(rows[0], vec!["2026-10-07", "2026-10-07", "2026", "2026-10-08"], "datetime golden mismatch: {:?}", rows[0]);
    let rows = rows_of(&exec, "SELECT CASE WHEN age > 29 THEN 'old' ELSE 'young' END, NULLIF(age, 30) FROM t WHERE id = 1");
    assert_eq!(rows[0], vec!["old", "NULL"], "conditional golden mismatch: {:?}", rows[0]);
    // Type errors -> TitanError, never panic.
    assert!(exec.execute("SELECT SUM(name) FROM t").is_err(), "SUM over text must error");
    assert!(exec.execute("SELECT 1 / 0 FROM t").is_err(), "division by zero must error");
}
