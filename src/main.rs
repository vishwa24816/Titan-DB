use std::sync::Arc;
use std::fs;
use std::path::Path;
use parking_lot::RwLock;

use titan_db::storage::pager::Pager;
use titan_db::catalog::Catalog;
use titan_db::sql::executor::Executor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 0. Cleanup
    if Path::new("titan_sql.db").exists() {
        fs::remove_file("titan_sql.db")?;
    }

    // 1. Initialize Engine
    let pager = Arc::new(Pager::open("titan_sql.db")?);
    let catalog = Arc::new(RwLock::new(Catalog::new()));
    let executor = Executor::new(pager, catalog);

    println!("TitanDB (Postgres-Compatible) Starting...");

    // 2. Execute SQL Deliverables
    
    // CREATE TABLE
    let create_sql = "CREATE TABLE users (id INT, name TEXT, age INT)";
    println!("SQL> {}", create_sql);
    println!("RES: {}", executor.execute(create_sql)?);

    // INSERT INTO
    let insert_sql = "INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)";
    println!("SQL> {}", insert_sql);
    println!("RES: {}", executor.execute(insert_sql)?);

    // SELECT
    let select_sql = "SELECT * FROM users WHERE age > 25";
    println!("SQL> {}", select_sql);
    println!("RES: {}", executor.execute(select_sql)?);

    // Demonstration of other requested features (Scaffolded)
    let alt_sql = "ALTER TABLE users ADD COLUMN email TEXT";
    println!("SQL> {}", alt_sql);
    println!("RES: {}", executor.execute(alt_sql)?);

    let drop_sql = "DROP TABLE users";
    println!("SQL> {}", drop_sql);
    println!("RES: {}", executor.execute(drop_sql)?);

    Ok(())
}
