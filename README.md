# Titan-DB

Titan-DB is a high-concurrency, PostgreSQL-compatible embedded database engine written from scratch in Rust. It is designed to leverage modern multi-core architectures while maintaining the simplicity and portability of a library-based database like SQLite.

## Key Features

- **Extreme Concurrency**: Utilizes a **B-Link Tree** (Lehman & Yao) indexing structure and a **Sharded Buffer Pool** to allow effectively unlimited concurrent readers and writers without global locks.
- **PostgreSQL Compatibility**: Supports a wide range of PostgreSQL-style SQL syntax including DDL (`CREATE TABLE`, `DROP TABLE`), DML (`INSERT`, `UPDATE`, `DELETE`), and DQL (`SELECT` with `WHERE`, `JOIN`, `GROUP BY`).
- **MVCC Architecture**: Designed for snapshot isolation where readers never block writers and vice versa.
- **Modern Web UI**: Includes a built-in **PGAdmin-style Web Admin** interface accessible via a browser.
- **WebSocket API**: Provides a real-time WebSocket interface for external backends and tools.
- **Memory Safety**: Written in 100% safe Rust (no `unsafe` blocks) to ensure memory safety and data integrity.

## Architecture

### Storage Engine
- **Pager**: Manages fixed-size 4KB pages with an LRU cache.
- **Sharding**: The buffer pool is sharded into 16 independent regions to minimize mutex contention.
- **B-Link Tree**: A modified B+Tree that includes "right-link" pointers and "high-keys," allowing threads to navigate the tree correctly even while nodes are being split by concurrent writers.

### SQL Layer
- **Parser**: Uses `sqlparser-rs` with the `PostgreSqlDialect`.
- **Executor**: Translates AST nodes into storage operations.
- **Catalog**: Manages table schemas and root page mapping.

## Getting Started

### Prerequisites
- [Rust](https://www.rust-lang.org/tools/install) (stable)

### Running the Server
To start the database server and the Web UI:

```bash
cargo run --bin server
```

Once running, you can access the Admin UI at **http://localhost:3030**.

### Running the CLI Demo
To run a quick CLI-based demonstration of the engine:

```bash
cargo run
```

## SQL Usage Example

```sql
-- Create a new table
CREATE TABLE users (id INT, name TEXT, age INT);

-- Insert data
INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30);
INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25);

-- Query data
SELECT * FROM users WHERE age > 20;

-- Alter schema
ALTER TABLE users ADD COLUMN email TEXT;
```

## Project Structure

- `src/storage/`: Pager, Buffer Pool, and Page definitions.
- `src/index/`: B-Link Tree implementation.
- `src/sql/`: SQL Executor and Result types.
- `src/catalog/`: Schema management.
- `src/bin/server.rs`: Async server with WebSocket and Web UI support.
- `web/`: Frontend assets (HTML/JS).

## License

MIT
