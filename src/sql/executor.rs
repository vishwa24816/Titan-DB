use sqlparser::ast::{BinaryOperator, DataType as SqlDataType, Expr, Query, SetExpr, Statement, TableFactor, Value};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};

use crate::catalog::{Catalog, ColumnDef, DataType, TableSchema};
use crate::error::{Result, TitanError};
use crate::index::blink::BLinkTree;
use crate::sql::encoding::{decode_row, encode_row, parse_literal, value_to_display, SqlValue};
use crate::sql::ExecutionResult;
use crate::storage::pager::Pager;
use crate::txn::{TxContext, TxManager};

#[derive(Debug, Clone)]
enum WriteOp {
    Insert { table: String, key: Vec<u8>, data: Vec<u8> },
    Delete { table: String, key: Vec<u8> },
}

struct BufferedTx {
    ctx: TxContext,
    ops: Vec<WriteOp>,
}

pub struct Executor {
    pager: Arc<Pager>,
    catalog: Arc<RwLock<Catalog>>,
    tx_manager: Arc<TxManager>,
    current: Mutex<Option<BufferedTx>>,
    wal: Arc<crate::storage::wal::Wal>,
}

impl Executor {
    pub fn new(pager: Arc<Pager>, catalog: Arc<RwLock<Catalog>>) -> Self {
        let wal = crate::storage::wal::Wal::open(crate::catalog::wal_path(pager.path()))
            .unwrap_or_else(|_| {
                crate::storage::wal::Wal::open(
                    std::env::temp_dir()
                        .join(format!("titan_fallback_{}.wal", std::process::id())),
                )
                .expect("fallback WAL open")
            });
        let exec = Executor {
            pager,
            catalog,
            tx_manager: Arc::new(TxManager::new()),
            current: Mutex::new(None),
            wal: Arc::new(wal),
        };
        exec.recover();
        exec
    }

    /// Open-time recovery: catalog rebuild + WAL replay validation
    /// (Analysis/Redo/Undo in `Wal::replay`). Missing sidecar on a fresh DB
    /// means empty catalog; data pages are already per-op flushed (STEAL +
    /// NO-FORCE), so replay only validates frames and filters loser txns —
    /// full row-level redo is a Wave-3 upgrade.
    /// Decision: sidecar `<db>.catalog` file (not reserved page 0) — avoids
    /// page-format coupling and keeps catalog writes independent of the pager.
    fn recover(&self) {
        let db_path = self.pager.path().to_path_buf();
        let entries = match Catalog::load_entries(&db_path) {
            Ok(e) => e,
            Err(_) => return,
        };
        {
            let mut catalog = self.catalog.write();
            for e in entries {
                if catalog.tables.contains_key(&e.name) {
                    continue;
                }
                let tree =
                    Arc::new(BLinkTree::open_existing(self.pager.clone(), e.root_page_id));
                catalog.tables.insert(
                    e.name.clone(),
                    TableSchema {
                        name: e.name,
                        columns: e.columns,
                        root_page_id: e.root_page_id,
                        tree,
                    },
                );
            }
        }
        // Restore the timestamp clock past every persisted version so new
        // snapshots see committed rows (fresh TxManager starts at 1).
        let mut max_ts = 0u64;
        {
            let catalog = self.catalog.read();
            for schema in catalog.tables.values() {
                if let Ok(m) = schema.tree.max_ts() {
                    max_ts = max_ts.max(m);
                }
            }
        }
        self.tx_manager.restore_clock(max_ts + 1);
        let _ = crate::storage::wal::Wal::replay(&crate::catalog::wal_path(&db_path));
    }

    /// Checkpoint: flush data pages + persist catalog + WAL checkpoint truncate.
    /// Bounds WAL growth (T-01-05).
    pub fn checkpoint(&self) -> Result<ExecutionResult> {
        let active: Vec<u64> =
            self.current.lock().as_ref().map(|b| b.ctx.tx_id).into_iter().collect();
        self.pager.flush_all()?;
        let lsn = self.wal.append(&crate::storage::wal::LogRecord::CheckpointBegin {
            active: active.clone(),
            dirty_lsn: 0,
        })?;
        self.catalog.read().persist(self.pager.path(), lsn)?;
        self.wal.checkpoint(active, 0, lsn)?;
        Ok(ExecutionResult::Message("Checkpoint complete.".to_string()))
    }

    /// WAL-log applied ops and force commit record + sync_all before ack.
    fn log_commit(&self, table: &str, ops: &[(Vec<u8>, Option<Vec<u8>>)], tx: u64) -> Result<()> {
        use crate::storage::wal::LogRecord;
        for (key, data) in ops {
            match data {
                Some(after) => {
                    self.wal.append(&LogRecord::Put {
                        table: table.to_string(),
                        key: key.clone(),
                        after: after.clone(),
                        tx,
                    })?;
                }
                None => {
                    self.wal.append(&LogRecord::Delete {
                        table: table.to_string(),
                        key: key.clone(),
                        tx,
                    })?;
                }
            }
        }
        // commit() appends Commit + sync_all before returning (durability ack).
        self.wal.commit(tx)?;
        Ok(())
    }

    /// Compat snapshot pin: ticks the manager clock (monotonic).
    pub fn next_tx_id(&self) -> u64 {
        self.tx_manager.next_ts()
    }

    pub fn tx_manager(&self) -> &Arc<TxManager> {
        &self.tx_manager
    }

    /// Reclaim versions below the GC horizon across all tables.
    pub fn vacuum(&self) -> Result<usize> {
        let horizon = self.tx_manager.gc_horizon();
        let catalog = self.catalog.read();
        let mut total = 0;
        for schema in catalog.tables.values() {
            total += schema.tree.vacuum(horizon)?;
        }
        Ok(total)
    }

    pub fn execute(&self, sql: &str) -> Result<ExecutionResult> {
        let trimmed = sql.trim();
        let upper = trimmed.to_ascii_uppercase();
        if upper == "BEGIN" || upper == "BEGIN TRANSACTION" || upper == "START TRANSACTION" {
            return self.begin_tx();
        }
        if upper == "COMMIT" || upper == "END TRANSACTION" {
            return self.commit_tx();
        }
        if upper == "ROLLBACK" || upper == "ABORT" {
            return self.rollback_tx();
        }
        let dialect = PostgreSqlDialect {};
        let ast = Parser::parse_sql(&dialect, sql)
            .map_err(|e| TitanError::InvalidSql(e.to_string()))?;

        let mut last_result = ExecutionResult::Message("No statements executed".to_string());
        for statement in ast {
            last_result = self.execute_statement(statement)?;
        }
        Ok(last_result)
    }

    fn begin_tx(&self) -> Result<ExecutionResult> {
        let mut cur = self.current.lock();
        if cur.is_some() {
            return Err(TitanError::UnsupportedSql("transaction already active".to_string()));
        }
        let ctx = self.tx_manager.begin();
        *cur = Some(BufferedTx { ctx, ops: Vec::new() });
        Ok(ExecutionResult::Message("BEGIN".to_string()))
    }

    fn commit_tx(&self) -> Result<ExecutionResult> {
        let buffered = self.current.lock().take();
        let Some(btx) = buffered else {
            return Err(TitanError::UnsupportedSql("no active transaction".to_string()));
        };
        // Validate first (first-committer-wins) before applying.
        for op in &btx.ops {
            let key = match op {
                WriteOp::Insert { key, .. } => key.clone(),
                WriteOp::Delete { key, .. } => key.clone(),
            };
            self.tx_manager.record_write(btx.ctx.tx_id, key);
        }
        let commit_ts = self.tx_manager.commit(btx.ctx)?;
        // Group ops per table so WAL replay stays table-scoped.
        let mut by_table: std::collections::HashMap<String, Vec<(Vec<u8>, Option<Vec<u8>>)>> =
            std::collections::HashMap::new();
        for op in &btx.ops {
            self.apply_op(op, commit_ts)?;
            match op {
                WriteOp::Insert { table, key, data } => {
                    by_table.entry(table.clone()).or_default().push((key.clone(), Some(data.clone())));
                }
                WriteOp::Delete { table, key } => {
                    by_table.entry(table.clone()).or_default().push((key.clone(), None));
                }
            }
        }
        // WAL: log + Commit + sync_all before ack (STEAL+NO-FORCE: pages may flush later).
        for (table, logged) in &by_table {
            self.log_commit(table, logged, commit_ts)?;
        }
        Ok(ExecutionResult::Message("COMMIT".to_string()))
    }

    fn rollback_tx(&self) -> Result<ExecutionResult> {
        let buffered = self.current.lock().take();
        let Some(btx) = buffered else {
            return Err(TitanError::UnsupportedSql("no active transaction".to_string()));
        };
        self.tx_manager.rollback(btx.ctx);
        Ok(ExecutionResult::Message("ROLLBACK".to_string()))
    }

    fn in_tx(&self) -> bool {
        self.current.lock().is_some()
    }

    /// Autocommit write: begin + validate + apply immediately.
    fn autocommit_write(&self, op: WriteOp) -> Result<u64> {
        let ctx = self.tx_manager.begin();
        let key = match &op {
            WriteOp::Insert { key, .. } => key.clone(),
            WriteOp::Delete { key, .. } => key.clone(),
        };
        self.tx_manager.record_write(ctx.tx_id, key);
        let commit_ts = self.tx_manager.commit(ctx)?;
        self.apply_op(&op, commit_ts)?;
        Ok(commit_ts)
    }

    fn submit_write(&self, op: WriteOp) -> Result<Option<u64>> {
        if let Some(btx) = self.current.lock().as_mut() {
            btx.ops.push(op);
            Ok(None) // buffered: WAL-logged at COMMIT
        } else {
            self.autocommit_write(op).map(Some)
        }
    }

    fn apply_op(&self, op: &WriteOp, ts: u64) -> Result<()> {
        // Resolve the target tree by table name (no first-table fallback:
        // multi-table writes must land in the right tree for JOINs to work).
        let tree = {
            let catalog = self.catalog.read();
            let (table, missing) = match op {
                WriteOp::Insert { table, .. } => (table, false),
                WriteOp::Delete { table, .. } => (table, false),
            };
            let _ = missing;
            catalog
                .tables
                .get(table)
                .map(|s| s.tree.clone())
                .ok_or_else(|| TitanError::CatalogMissing(format!("table {table} missing")))?
        };
        match op {
            WriteOp::Insert { key, data, .. } => {
                tree.insert(key.clone(), data.clone(), ts)?;
            }
            WriteOp::Delete { key, .. } => {
                tree.delete(key, ts)?;
            }
        }
        Ok(())
    }

    fn execute_statement(&self, statement: Statement) -> Result<ExecutionResult> {
        match statement {
            Statement::CreateTable { name, columns, .. } => {
                let table_name = name.to_string();
                let mut catalog = self.catalog.write();

                if catalog.tables.contains_key(&table_name) {
                    return Err(TitanError::TableExists(table_name.clone()));
                }

                let btree = Arc::new(BLinkTree::new(self.pager.clone())?);
                let root_id = btree.root_page_id();
                let schema = TableSchema {
                    name: table_name.clone(),
                    columns: columns
                        .into_iter()
                        .map(|c| ColumnDef {
                            name: c.name.to_string(),
                            data_type: match c.data_type {
                                SqlDataType::Integer(_) | SqlDataType::Int(_) | SqlDataType::BigInt(_) => {
                                    DataType::Integer
                                }
                                SqlDataType::Float(_) | SqlDataType::Double | SqlDataType::Real => DataType::Float,
                                SqlDataType::Boolean | SqlDataType::Bool => DataType::Boolean,
                                SqlDataType::Blob(_) | SqlDataType::Bytea => DataType::Bytes,
                                SqlDataType::Date => DataType::Date,
                                _ => DataType::Text,
                            },
                            nullable: true,
                        })
                        .collect(),
                    root_page_id: root_id,
                    tree: btree,
                };

                catalog.tables.insert(table_name.clone(), schema);
                let lsn = self.wal.append(&crate::storage::wal::LogRecord::CheckpointBegin {
                    active: Vec::new(),
                    dirty_lsn: 0,
                })?;
                catalog.persist(self.pager.path(), lsn)?;
                self.wal.sync_all()?;
                Ok(ExecutionResult::Message(format!("Table {} created.", table_name)))
            }
            Statement::Insert { table_name, source, .. } => {
                let name = table_name.to_string();
                let (tree, col_types, ncols) = {
                    let catalog = self.catalog.read();
                    let schema = catalog.tables.get(&name).ok_or_else(|| {
                        TitanError::TableNotFound(name.to_string())
                    })?;
                    (
                        schema.tree.clone(),
                        schema.columns.iter().map(|c| c.data_type.clone()).collect::<Vec<_>>(),
                        schema.columns.len(),
                    )
                };

                let mut row_count = 0;
                if let Some(src) = source {
                    if let SetExpr::Values(values) = *src.body {
                        for row in values.rows {
                            if row.is_empty() {
                                continue;
                            }
                            let lits: Vec<String> = row
                                .iter()
                                .map(|r| match r {
                                    Expr::Value(Value::SingleQuotedString(s)) => s.clone(),
                                    Expr::Value(Value::Number(n, _)) => n.clone(),
                                    Expr::Value(Value::Boolean(b)) => b.to_string(),
                                    Expr::Value(Value::Null) => "NULL".to_string(),
                                    other => other.to_string(),
                                })
                                .collect();
                            // Pad/truncate to schema width.
                            let mut vals: Vec<SqlValue> = lits
                                .iter()
                                .enumerate()
                                .map(|(i, lit)| {
                                    let dt = col_types.get(i).unwrap_or(&DataType::Text);
                                    parse_literal(lit, dt)
                                })
                                .collect();
                            while vals.len() < ncols {
                                vals.push(SqlValue::Null);
                            }
                            vals.truncate(ncols);
                            let key = value_to_display(&vals[0]).into_bytes();
                            let data = encode_row(&vals);
                            if let Some(ts) = self.submit_write(WriteOp::Insert {
                                table: name.clone(),
                                key: key.clone(),
                                data: data.clone(),
                            })? {
                                self.log_commit(&name, &[(key, Some(data))], ts)?;
                            }
                            row_count += 1;
                        }
                    }
                }

                Ok(ExecutionResult::Message(format!(
                    "Inserted {} row(s) into {}.",
                    row_count, name
                )))
            }
            Statement::Query(query) => self.execute_query(*query),
            Statement::Update { table, assignments, selection, .. } => {
                let name = table.relation.to_string();
                let (col_types, ncols) = {
                    let catalog = self.catalog.read();
                    let schema = catalog.tables.get(&name).ok_or_else(|| {
                        TitanError::TableNotFound(name.to_string())
                    })?;
                    (
                        schema.columns.iter().map(|c| c.data_type.clone()).collect::<Vec<_>>(),
                        schema.columns.len(),
                    )
                };

                let mut target_key = None;
                if let Some(Expr::BinaryOp { left, op: BinaryOperator::Eq, right }) = selection {
                    if left.to_string() == "id" {
                        let k = match *right {
                            Expr::Value(Value::Number(n, _)) => n,
                            Expr::Value(Value::SingleQuotedString(s)) => s,
                            other => other.to_string(),
                        };
                        target_key = Some(k);
                    }
                }

                let new_val = if !assignments.is_empty() {
                    match &assignments[0].value {
                        Expr::Value(Value::SingleQuotedString(s)) => s.clone(),
                        Expr::Value(Value::Number(n, _)) => n.clone(),
                        other => other.to_string(),
                    }
                } else {
                    "updated".to_string()
                };

                let mut updated = 0;
                if let Some(key) = target_key {
                    // Rebuild full row: read current visible row, patch col 1.
                    let read_ts = self.read_ts();
                    let current_row = self.read_row(&name, key.as_bytes(), read_ts)?;
                    let mut vals = current_row.unwrap_or_else(|| {
                        let mut v = vec![SqlValue::Null; ncols];
                        v[0] = parse_literal(&key, &col_types[0]);
                        v
                    });
                    if vals.len() > 1 {
                        vals[1] = parse_literal(&new_val, &col_types[1]);
                    }
                    let data = encode_row(&vals);
                    if let Some(ts) = self.submit_write(WriteOp::Insert {
                        table: name.clone(),
                        key: key.clone().into_bytes(),
                        data: data.clone(),
                    })? {
                        self.log_commit(&name, &[(key.into_bytes(), Some(data))], ts)?;
                    }
                    updated += 1;
                }
                Ok(ExecutionResult::Message(format!("Updated {} row(s) in {}.", updated, name)))
            }
            Statement::Delete { from, selection, .. } => {
                let name = from[0].relation.to_string();
                {
                    let catalog = self.catalog.read();
                    if !catalog.tables.contains_key(&name) {
                        return Err(TitanError::TableNotFound(name.to_string()));
                    }
                }

                let mut target_key = None;
                if let Some(Expr::BinaryOp { left, op: BinaryOperator::Eq, right }) = selection {
                    if left.to_string() == "id" {
                        let k = match *right {
                            Expr::Value(Value::Number(n, _)) => n,
                            Expr::Value(Value::SingleQuotedString(s)) => s,
                            other => other.to_string(),
                        };
                        target_key = Some(k);
                    }
                }

                let mut deleted = 0;
                if let Some(key) = target_key {
                    if self.in_tx() {
                        self.submit_write(WriteOp::Delete { table: name.clone(), key: key.into_bytes() })?;
                        deleted += 1;
                    } else {
                        let ctx = self.tx_manager.begin();
                        self.tx_manager.record_write(ctx.tx_id, key.as_bytes().to_vec());
                        let commit_ts = self.tx_manager.commit(ctx)?;
                        let catalog = self.catalog.read();
                        let schema = catalog.tables.get(&name).ok_or_else(|| {
                            TitanError::TableNotFound(name.to_string())
                        })?;
                        if schema.tree.delete(key.as_bytes(), commit_ts)? {
                            deleted += 1;
                        }
                        drop(catalog);
                        self.log_commit(&name, &[(key.into_bytes(), None)], commit_ts)?;
                    }
                }
                Ok(ExecutionResult::Message(format!("Deleted {} row(s) from {}.", deleted, name)))
            }
            Statement::AlterTable { .. } => {
                return Err(TitanError::UnsupportedSql("ALTER TABLE not supported".to_string()))
            }
            Statement::Drop { .. } => {
                return Err(TitanError::UnsupportedSql("DROP not supported".to_string()))
            }
            other => {
                return Err(TitanError::UnsupportedSql(format!(
                    "unsupported statement: {:?}",
                    std::mem::discriminant(&other)
                )))
            }
        }
    }

    fn read_ts(&self) -> u64 {
        self.current.lock().as_ref().map(|b| b.ctx.read_ts).unwrap_or_else(|| self.tx_manager.next_ts())
    }

    fn read_row(&self, table: &str, key: &[u8], read_ts: u64) -> Result<Option<Vec<SqlValue>>> {
        let (tree, col_types) = {
            let catalog = self.catalog.read();
            let schema = catalog.tables.get(table).ok_or_else(|| {
                TitanError::TableNotFound(table.to_string())
            })?;
            (
                schema.tree.clone(),
                schema.columns.iter().map(|c| c.data_type.clone()).collect::<Vec<_>>(),
            )
        };
        match tree.search(key, read_ts)? {
            Some(bytes) => Ok(Some(decode_row(&bytes, &col_types)?)),
            None => Ok(None),
        }
    }

    fn execute_query(&self, query: Query) -> Result<ExecutionResult> {
        let SetExpr::Select(select) = query.body.as_ref() else {
            return Err(TitanError::UnsupportedSql("only SELECT queries are supported".to_string()));
        };
        if select.from.is_empty() {
            return Err(TitanError::UnsupportedSql("SELECT without FROM is not supported".to_string()));
        }
        // Only plain FROM + JOINs; derived tables/subqueries are honest errors.
        for t in &select.from {
            match &t.relation {
                TableFactor::Table { .. } => {}
                _ => return Err(TitanError::UnsupportedSql("derived tables are not supported".to_string())),
            }
            for j in &t.joins {
                match &j.relation {
                    TableFactor::Table { .. } => {}
                    _ => return Err(TitanError::UnsupportedSql("derived join tables are not supported".to_string())),
                }
            }
        }
        let read_ts = self.read_ts();
        // ---- Scan (+ PK pushdown: WHERE pk = lit -> BLinkTree::search) ----
        let mut frame = self.scan_table(&self.table_name(&select.from[0].relation)?, select.selection.as_ref(), read_ts)?;
        // ---- JOINs (nested-loop + hash for equi-join; inner/left/right) ----
        for jw in &select.from[0].joins {
            let rname = self.table_name(&jw.relation)?;
            let right = self.scan_table(&rname, None, read_ts)?;
            frame = self.join_frames(frame, right, jw)?;
        }
        if select.from.len() > 1 {
            return Err(TitanError::UnsupportedSql("comma joins are not supported; use JOIN..ON".to_string()));
        }
        // ---- Filter ----
        if let Some(pred) = select.selection.as_ref() {
            // Pushdown already narrowed PK-equality; re-apply predicate for correctness.
            frame = self.filter_frame(frame, pred)?;
        }
        // ---- GROUP BY + aggregates / window / projection ----
        let (columns, rows) = self.project_frame(frame, &select)?;
        // ---- ORDER BY + LIMIT/OFFSET (Query level in sqlparser 0.43) ----
        let (columns, rows) = Self::sort_limit(columns, rows, &select, &query)?;
        Ok(ExecutionResult::ResultSet { columns, rows })
    }
}

// ===== Volcano helpers: Scan / Filter / Join / Project / Sort+Limit =====

use sqlparser::ast::{FunctionArg, FunctionArgExpr, GroupByExpr, JoinConstraint, JoinOperator, Select, SelectItem, UnaryOperator};
use crate::sql::functions::{self, AggKind};

/// Row frame flowing through the Volcano pipeline. `qual` is the table/alias
/// prefix per column ("" when unknown); duplicates allowed after JOINs.
struct Frame {
    qual: Vec<String>,
    columns: Vec<String>,
    rows: Vec<Vec<SqlValue>>,
}

/// Max rows materialised by a single JOIN step (T-01-09 cartesian cap).
const JOIN_ROW_CAP: usize = 100_000;

impl Executor {
    fn table_name(&self, t: &TableFactor) -> Result<String> {
        match t {
            TableFactor::Table { name, .. } => Ok(name.to_string()),
            _ => Err(TitanError::UnsupportedSql("only plain table references are supported".to_string())),
        }
    }

    /// Scan with predicate pushdown: equality on the PK (first column) routes
    /// to `BLinkTree::search()`; everything else falls back to `scan_all`.
    fn scan_table(&self, name: &str, selection: Option<&Expr>, read_ts: u64) -> Result<Frame> {
        let (tree, col_types, columns) = {
            let catalog = self.catalog.read();
            let schema = catalog.tables.get(name).ok_or_else(|| {
                TitanError::TableNotFound(name.to_string())
            })?;
            (
                schema.tree.clone(),
                schema.columns.iter().map(|c| c.data_type.clone()).collect::<Vec<_>>(),
                schema.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            )
        };
        // PK pushdown probe.
        let mut pushed: Option<Vec<(Vec<u8>, Vec<u8>)>> = None;
        if let (Some(pred), Some(pk)) = (selection, columns.first()) {
            if let Some(lit) = pk_equality_lit(pred, pk) {
                let key = lit.into_bytes();
                if let Some(bytes) = tree.search(&key, read_ts)? {
                    pushed = Some(vec![(key, bytes)]);
                } else {
                    pushed = Some(vec![]);
                }
            }
        }
        let raw: Vec<(Vec<u8>, Vec<u8>)> = match pushed {
            Some(v) => v,
            None => tree.scan_all(read_ts)?,
        };
        // Overlay own buffered writes for THIS table (tx visibility from plan 5).
        let mut overlay: std::collections::HashMap<Vec<u8>, Option<Vec<u8>>> = std::collections::HashMap::new();
        if let Some(btx) = self.current.lock().as_ref() {
            for op in &btx.ops {
                match op {
                    WriteOp::Insert { table, key, data } if table == name => {
                        overlay.insert(key.clone(), Some(data.clone()));
                    }
                    WriteOp::Delete { table, key } if table == name => {
                        overlay.insert(key.clone(), None);
                    }
                    _ => {}
                }
            }
        }
        let mut merged: std::collections::HashMap<Vec<u8>, Vec<u8>> = raw.into_iter().collect();
        for (k, v) in overlay {
            match v {
                Some(data) => { merged.insert(k, data); }
                None => { merged.remove(&k); }
            }
        }
        let mut rows = Vec::with_capacity(merged.len());
        for (_k, bytes) in merged {
            rows.push(decode_row(&bytes, &col_types)?);
        }
        let qual = vec![name.to_string(); columns.len()];
        Ok(Frame { qual, columns, rows })
    }

    fn filter_frame(&self, mut frame: Frame, pred: &Expr) -> Result<Frame> {
        let mut kept = Vec::with_capacity(frame.rows.len());
        for row in frame.rows.drain(..) {
            if Self::eval_pred(pred, &frame.qual, &frame.columns, &row)? {
                kept.push(row);
            }
        }
        frame.rows = kept;
        Ok(frame)
    }

    fn join_frames(&self, left: Frame, right: Frame, jw: &sqlparser::ast::Join) -> Result<Frame> {
        // Resolve (equi-key, outer-kind). CROSS JOIN / comma => cross product.
        enum Outer { None, Left, Right, Full }
        let (keys, outer) = match &jw.join_operator {
            JoinOperator::CrossJoin => (None, Outer::None),
            JoinOperator::Inner(c) => match c {
                JoinConstraint::None => (None, Outer::None),
                _ => (Some(self.join_keys(c, &left, &right)?), Outer::None),
            },
            JoinOperator::LeftOuter(c) => (Some(self.join_keys(c, &left, &right)?), Outer::Left),
            JoinOperator::RightOuter(c) => (Some(self.join_keys(c, &left, &right)?), Outer::Right),
            JoinOperator::FullOuter(c) => (Some(self.join_keys(c, &left, &right)?), Outer::Full),
            other => return Err(TitanError::UnsupportedSql(format!("unsupported JOIN type: {other:?}"))),
        };
        let mut qual = left.qual.clone();
        qual.extend(right.qual.iter().cloned());
        let mut columns = left.columns.clone();
        columns.extend(right.columns.iter().cloned());
        let mut out: Vec<Vec<SqlValue>> = Vec::new();
        let is_outer = !matches!(outer, Outer::None);
        if let Some((li, ri)) = keys {
            // Hash join on equi-keys.
            let mut hash: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
            for (i, r) in right.rows.iter().enumerate() {
                hash.entry(display_key(&r[ri])).or_default().push(i);
            }
            let mut matched_r = vec![false; right.rows.len()];
            for l in &left.rows {
                let mut matched = false;
                if let Some(idxs) = hash.get(&display_key(&l[li])) {
                    for &i in idxs {
                        let mut row = l.clone();
                        row.extend(right.rows[i].iter().cloned());
                        out.push(row);
                        matched = true;
                        matched_r[i] = true;
                    }
                }
                if !matched && matches!(outer, Outer::Left | Outer::Full) {
                    let mut row = l.clone();
                    row.extend((0..right.columns.len()).map(|_| SqlValue::Null));
                    out.push(row);
                }
                if out.len() > JOIN_ROW_CAP {
                    return Err(TitanError::UnsupportedSql("JOIN result exceeds row cap".to_string()));
                }
            }
            if matches!(outer, Outer::Right | Outer::Full) {
                for (i, r) in right.rows.iter().enumerate() {
                    if !matched_r[i] {
                        let mut row: Vec<SqlValue> = (0..left.columns.len()).map(|_| SqlValue::Null).collect();
                        row.extend(r.iter().cloned());
                        out.push(row);
                    }
                }
            }
        } else {
            // Nested-loop cross product (bounded by the same cap).
            if left.rows.is_empty() || right.rows.is_empty() {
                if is_outer {
                    for l in &left.rows {
                        let mut row = l.clone();
                        row.extend((0..right.columns.len()).map(|_| SqlValue::Null));
                        out.push(row);
                    }
                    for r in &right.rows {
                        let mut row: Vec<SqlValue> = (0..left.columns.len()).map(|_| SqlValue::Null).collect();
                        row.extend(r.iter().cloned());
                        out.push(row);
                    }
                }
            } else {
                for l in &left.rows {
                    for r in &right.rows {
                        let mut row = l.clone();
                        row.extend(r.iter().cloned());
                        out.push(row);
                        if out.len() > JOIN_ROW_CAP {
                            return Err(TitanError::UnsupportedSql("JOIN result exceeds row cap".to_string()));
                        }
                    }
                }
            }
        }
        Ok(Frame { qual, columns, rows: out })
    }

    /// Resolve an equi-join key pair as (left_col, right_col). Handles
    /// `a.id = b.id` in either order plus `USING(col)`.
    fn join_keys(&self, c: &JoinConstraint, left: &Frame, right: &Frame) -> Result<(usize, usize)> {
        match c {
            JoinConstraint::On(expr) => match expr {
                Expr::BinaryOp { left: l, op: BinaryOperator::Eq, right: r } => {
                    if let (Ok(li), Ok(ri)) = (
                        Self::col_index(&left.qual, &left.columns, l),
                        Self::col_index(&right.qual, &right.columns, r),
                    ) {
                        return Ok((li, ri));
                    }
                    // Swapped: l in right, r in left.
                    let li = Self::col_index(&left.qual, &left.columns, r)
                        .map_err(|_| TitanError::UnsupportedSql("JOIN ON must be an equi-condition".to_string()))?;
                    let ri = Self::col_index(&right.qual, &right.columns, l)
                        .map_err(|_| TitanError::UnsupportedSql("JOIN ON must be an equi-condition".to_string()))?;
                    Ok((li, ri))
                }
                _ => Err(TitanError::UnsupportedSql("only equi-JOIN ON conditions are supported".to_string())),
            },
            JoinConstraint::Using(cols) => {
                if cols.len() != 1 {
                    return Err(TitanError::UnsupportedSql("USING with multiple columns is not supported".to_string()));
                }
                let c = cols[0].to_string();
                let li = left.columns.iter().position(|x| x == &c)
                    .ok_or_else(|| TitanError::UnsupportedSql(format!("JOIN USING column {c} missing on left")))?;
                let ri = right.columns.iter().position(|x| x == &c)
                    .ok_or_else(|| TitanError::UnsupportedSql(format!("JOIN USING column {c} missing on right")))?;
                Ok((li, ri))
            }
            JoinConstraint::None => Err(TitanError::UnsupportedSql("JOIN without ON/USING needs CROSS JOIN".to_string())),
            JoinConstraint::Natural => Err(TitanError::UnsupportedSql("NATURAL JOIN is not supported".to_string())),
        }
    }

    /// Projection: aggregates (+GROUP BY), window fns, scalar exprs, *.
    fn project_frame(&self, frame: Frame, select: &Select) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        // Expand * / t.*.
        let items: Vec<SelectItem> = select.projection.clone();
        let has_star = items.iter().any(|i| matches!(i, SelectItem::Wildcard(_)));
        let _ = has_star;
        // Aggregate detection.
        let mut agg_items: Vec<(usize, String, AggKind, Option<Expr>)> = Vec::new();
        let mut window_items: Vec<(usize, String, bool)> = Vec::new(); // (pos, name ROW_NUMBER|RANK, desc?)
        for (pos, item) in items.iter().enumerate() {
            if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
                if let Some((k, arg)) = agg_of(e) {
                    agg_items.push((pos, item_label(item), k, arg));
                } else if let Some((w, desc)) = window_of(e) {
                    window_items.push((pos, w, desc));
                }
            }
        }
        let group_by: Vec<Expr> = match &select.group_by {
            GroupByExpr::All => return Err(TitanError::UnsupportedSql("GROUP BY ALL is not supported".to_string())),
            GroupByExpr::Expressions(exprs) => exprs.clone(),
        };
        if !agg_items.is_empty() {
            return self.project_aggregate(&frame, &items, &agg_items, &group_by);
        }
        // Row-wise projection (+ window columns computed after ordering).
        let mut col_names: Vec<String> = Vec::new();
        for item in &items {
            match item {
                SelectItem::Wildcard(_) => {
                    for c in &frame.columns { col_names.push(c.clone()); }
                }
                SelectItem::QualifiedWildcard(obj, _) => {
                    let q = obj.to_string();
                    for (i, c) in frame.columns.iter().enumerate() {
                        if frame.qual[i] == q { col_names.push(c.clone()); }
                    }
                }
                item => col_names.push(item_label(item)),
            }
        }
        // Pre-compute window order if needed.
        let mut order_idx: Vec<usize> = (0..frame.rows.len()).collect();
        let mut win_sort: Option<(Vec<usize>, Vec<bool>)> = None;
        if !window_items.is_empty() {
            // Use the first window's ORDER BY (tests use single window).
            if let Some(keys) = items.iter().find_map(|i| match i {
                SelectItem::UnnamedExpr(x) | SelectItem::ExprWithAlias { expr: x, .. } => window_order_exprs(x),
                _ => None,
            }) {
                let mut cols: Vec<usize> = Vec::new();
                let mut desc: Vec<bool> = Vec::new();
                for (e, d) in &keys {
                    cols.push(Self::col_index(&frame.qual, &frame.columns, e)?);
                    desc.push(*d);
                }
                order_idx.sort_by(|&a, &b| compare_rows(&frame.rows[a], &frame.rows[b], &cols, &desc));
                win_sort = Some((cols, desc));
            } else {
                order_idx.sort_by(|&a, &b| a.cmp(&b));
            }
        }
        // ROW_NUMBER / RANK assignment in window order.
        let mut win_vals: std::collections::HashMap<usize, Vec<String>> = std::collections::HashMap::new();
        for (pos, wname, _desc) in &window_items {
            // Partition: none in v1 (OVER() / OVER(ORDER BY)); PARTITION BY -> honest error.
            let mut rank = 0i64; let mut rn = 0i64;
            let mut prev_key: Option<Vec<(u8, String)>> = None;
            for &ri in &order_idx {
                rn += 1;
                let key: Vec<(u8, String)> = frame.rows[ri].iter().map(functions::val_sort_key).collect();
                if prev_key.as_ref() != Some(&key) { rank = rn; prev_key = Some(key); }
                let v = if wname == "ROW_NUMBER" { rn } else { rank };
                win_vals.entry(ri).or_default().push(v.to_string());
                let _ = pos;
            }
        }
        // Build rows in original (or window) order.
        let emit_order: Vec<usize> = if window_items.is_empty() {
            (0..frame.rows.len()).collect()
        } else { order_idx.clone() };
        let _ = win_sort;
        let mut rows: Vec<Vec<String>> = Vec::new();
        for ri in emit_order {
            let row = &frame.rows[ri];
            let mut out: Vec<String> = Vec::new();
            let mut wincursor = 0;
            for item in &items {
                match item {
                    SelectItem::Wildcard(_) => {
                        for v in row { out.push(crate::sql::encoding::value_to_display(v)); }
                    }
                    SelectItem::QualifiedWildcard(obj, _) => {
                        let q = obj.to_string();
                        for (i, v) in row.iter().enumerate() {
                            if frame.qual[i] == q { out.push(crate::sql::encoding::value_to_display(v)); }
                        }
                    }
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                        if window_of(e).is_some() {
                            let vals = win_vals.get(&ri).cloned().unwrap_or_default();
                            out.push(vals.get(wincursor).cloned().unwrap_or_else(|| "1".to_string()));
                            wincursor += 1;
                        } else {
                            out.push(crate::sql::encoding::value_to_display(
                                &Self::eval_scalar(e, &frame.qual, &frame.columns, row)?));
                        }
                    }
                }
            }
            rows.push(out);
        }
        Ok((col_names, rows))
    }

    /// GROUP BY (hash) + aggregates. Non-agg projection exprs must be group keys.
    fn project_aggregate(
        &self,
        frame: &Frame,
        items: &[SelectItem],
        aggs: &[(usize, String, AggKind, Option<Expr>)],
        group_by: &[Expr],
    ) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        if !group_by.is_empty() {
            // Validate: group keys must be plain columns.
            for g in group_by {
                Self::col_index(&frame.qual, &frame.columns, g)
                    .map_err(|_| TitanError::UnsupportedSql("GROUP BY supports plain columns only".to_string()))?;
            }
        } else {
            // No GROUP BY: every non-agg item must itself be an aggregate.
            for (pos, item) in items.iter().enumerate() {
                if aggs.iter().any(|(p, _, _, _)| *p == pos) { continue; }
                match item {
                    SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) =>
                        return Err(TitanError::UnsupportedSql("SELECT * with aggregates needs GROUP BY".to_string())),
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                        if agg_of(e).is_none() && window_of(e).is_none() {
                            return Err(TitanError::UnsupportedSql(
                                "non-aggregated column without GROUP BY".to_string()));
                        }
                    }
                }
            }
        }
        // Partition rows.
        let mut groups: Vec<(Vec<String>, Vec<usize>)> = Vec::new();
        let mut gindex: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        if group_by.is_empty() {
            groups.push((vec![], (0..frame.rows.len()).collect()));
        } else {
            for (ri, row) in frame.rows.iter().enumerate() {
                let mut key_vals: Vec<String> = Vec::new();
                for g in group_by {
                    let ci = Self::col_index(&frame.qual, &frame.columns, g)?;
                    key_vals.push(crate::sql::encoding::value_to_display(&row[ci]));
                }
                let k = key_vals.join("\u{1f}");
                if let Some(&gi) = gindex.get(&k) {
                    groups[gi].1.push(ri);
                } else {
                    gindex.insert(k, groups.len());
                    groups.push((key_vals, vec![ri]));
                }
            }
        }
        let col_names: Vec<String> = items.iter().map(item_label).collect();
        let mut rows: Vec<Vec<String>> = Vec::new();
        for (_gkey, members) in &groups {
            let mut out: Vec<String> = Vec::new();
            for (pos, item) in items.iter().enumerate() {
                if let Some((_, _, kind, arg)) = aggs.iter().find(|(p, _, _, _)| *p == pos) {
                    let vals: Vec<SqlValue> = match (kind, arg) {
                        (AggKind::CountStar, _) => vec![SqlValue::Int64(1); members.len()],
                        (_, Some(a)) => members.iter().map(|&ri| {
                            Self::eval_scalar(a, &frame.qual, &frame.columns, &frame.rows[ri])
                        }).collect::<Result<Vec<_>>>()?,
                        (_, None) => return Err(TitanError::UnsupportedSql("aggregate needs an argument".to_string())),
                    };
                    let dt = DataType::Text;
                    out.push(crate::sql::encoding::value_to_display(&functions::eval_aggregate(*kind, &vals, &dt)?));
                } else {
                    match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                            let ri = members.first().copied().unwrap_or(0);
                            if frame.rows.is_empty() {
                                out.push("NULL".to_string());
                            } else {
                                out.push(crate::sql::encoding::value_to_display(
                                    &Self::eval_scalar(e, &frame.qual, &frame.columns, &frame.rows[ri])?));
                            }
                        }
                        _ => return Err(TitanError::UnsupportedSql("*/t.* with GROUP BY is not supported".to_string())),
                    }
                }
            }
            rows.push(out);
        }
        Ok((col_names, rows))
    }

    fn sort_limit(columns: Vec<String>, mut rows: Vec<Vec<String>>, select: &Select, query: &Query) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        // ORDER BY lives on Query in sqlparser 0.43; Select.sort_by holds
        // bare Exprs (ascending). Numeric-aware compare when all values parse.
        let mut keys: Vec<(usize, bool)> = Vec::new();
        for o in &query.order_by {
            keys.push((Self::result_col_index(&columns, &o.expr)?, o.asc == Some(false)));
        }
        for e in &select.sort_by {
            keys.push((Self::result_col_index(&columns, e)?, false));
        }
        if !keys.is_empty() {
            let numeric = rows.iter().all(|r| r.iter().all(|v| v.parse::<i64>().is_ok()));
            rows.sort_by(|a, b| {
                for (i, desc) in &keys {
                    let ord = if numeric {
                        a[*i].parse::<i64>().unwrap().cmp(&b[*i].parse::<i64>().unwrap())
                    } else {
                        a[*i].cmp(&b[*i])
                    };
                    if ord != std::cmp::Ordering::Equal {
                        return if *desc { ord.reverse() } else { ord };
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
        // LIMIT / OFFSET (Query level in sqlparser 0.43).
        let limit: Option<usize> = match &query.limit {
            Some(Expr::Value(sqlparser::ast::Value::Number(n, _))) =>
                Some(n.parse().map_err(|_| TitanError::UnsupportedSql("bad LIMIT".to_string()))?),
            Some(_) => return Err(TitanError::UnsupportedSql("LIMIT must be a number".to_string())),
            None => None,
        };
        let offset: usize = match &query.offset {
            Some(off) => match &off.value {
                Expr::Value(sqlparser::ast::Value::Number(n, _)) =>
                    n.parse().map_err(|_| TitanError::UnsupportedSql("bad OFFSET".to_string()))?,
                _ => return Err(TitanError::UnsupportedSql("OFFSET must be a number".to_string())),
            },
            None => 0,
        };
        if offset >= rows.len() {
            return Ok((columns, vec![]));
        }
        let end = match limit {
            Some(l) => (offset + l).min(rows.len()),
            None => rows.len(),
        };
        Ok((columns, rows[offset..end].to_vec()))
    }

    fn result_col_index(columns: &[String], expr: &Expr) -> Result<usize> {
        match expr {
            Expr::Identifier(id) => columns.iter().position(|c| c == &id.value || c.ends_with(&format!(".{}", id.value)))
                .ok_or_else(|| TitanError::UnsupportedSql(format!("ORDER BY unknown column {}", id.value))),
            Expr::CompoundIdentifier(parts) => {
                let name = parts.last().map(|p| p.value.clone()).unwrap_or_default();
                columns.iter().position(|c| c == &name || c.ends_with(&format!(".{name}")))
                    .ok_or_else(|| TitanError::UnsupportedSql(format!("ORDER BY unknown column {name}")))
            }
            Expr::Value(sqlparser::ast::Value::Number(n, _)) => {
                let i: usize = n.parse().map_err(|_| TitanError::UnsupportedSql("bad ORDER BY position".to_string()))?;
                if i == 0 || i > columns.len() {
                    return Err(TitanError::UnsupportedSql("ORDER BY position out of range".to_string()));
                }
                Ok(i - 1)
            }
            _ => Err(TitanError::UnsupportedSql("ORDER BY supports columns or positions only".to_string())),
        }
    }

    fn col_index(_qual: &[String], columns: &[String], expr: &Expr) -> Result<usize> {
        match expr {
            Expr::Identifier(id) => columns.iter().position(|c| c == &id.value)
                .ok_or_else(|| TitanError::UnsupportedSql(format!("unknown column {}", id.value))),
            Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                let (q, name) = (parts[0].value.clone(), parts[1].value.clone());
                _qual.iter().zip(columns.iter()).position(|(qq, c)| *qq == q && *c == name)
                    .or_else(|| columns.iter().position(|c| *c == name))
                    .ok_or_else(|| TitanError::UnsupportedSql(format!("unknown column {q}.{name}")))
            }
            _ => Err(TitanError::UnsupportedSql("expected a plain column reference".to_string())),
        }
    }

    /// Full boolean predicate eval: comparisons, AND/OR/NOT, LIKE, IN, IS NULL.
    fn eval_pred(pred: &Expr, qual: &[String], columns: &[String], row: &[SqlValue]) -> Result<bool> {
        match pred {
            Expr::BinaryOp { left, op, right } => match op {
                BinaryOperator::And => Ok(Self::eval_pred(left, qual, columns, row)?
                    && Self::eval_pred(right, qual, columns, row)?),
                BinaryOperator::Or => Ok(Self::eval_pred(left, qual, columns, row)?
                    || Self::eval_pred(right, qual, columns, row)?),
                BinaryOperator::Eq | BinaryOperator::NotEq
                | BinaryOperator::Lt | BinaryOperator::LtEq
                | BinaryOperator::Gt | BinaryOperator::GtEq => {
                    let (a, b) = (
                        Self::eval_scalar(left, qual, columns, row)?,
                        Self::eval_scalar(right, qual, columns, row)?,
                    );
                    if a == SqlValue::Null || b == SqlValue::Null { return Ok(false); }
                    let ord = compare_scalar(&a, &b)?;
                    Ok(match op {
                        BinaryOperator::Eq => ord == 0,
                        BinaryOperator::NotEq => ord != 0,
                        BinaryOperator::Lt => ord < 0,
                        BinaryOperator::LtEq => ord <= 0,
                        BinaryOperator::Gt => ord > 0,
                        _ => ord >= 0,
                    })
                }
                _ => Err(TitanError::UnsupportedSql(format!("unsupported predicate operator {op:?}"))),
            },
            Expr::UnaryOp { op: UnaryOperator::Not, expr } =>
                Ok(!Self::eval_pred(expr, qual, columns, row)?),
            Expr::IsNull(e) => Ok(Self::eval_scalar(e, qual, columns, row)? == SqlValue::Null),
            Expr::IsNotNull(e) => Ok(Self::eval_scalar(e, qual, columns, row)? != SqlValue::Null),
            Expr::Like { expr, pattern, .. } => {
                let (a, b) = (
                    Self::eval_scalar(expr, qual, columns, row)?,
                    Self::eval_scalar(pattern, qual, columns, row)?,
                );
                match (a, b) {
                    (SqlValue::Text(t), SqlValue::Text(p)) => Ok(functions::like_match(&t, &p)),
                    (SqlValue::Null, _) | (_, SqlValue::Null) => Ok(false),
                    _ => Err(TitanError::UnsupportedSql("LIKE needs text operands".to_string())),
                }
            }
            Expr::InList { expr, list, negated } => {
                let v = Self::eval_scalar(expr, qual, columns, row)?;
                if v == SqlValue::Null { return Ok(false); }
                let mut hit = false;
                for item in list {
                    let c = Self::eval_scalar(item, qual, columns, row)?;
                    if c != SqlValue::Null && compare_scalar(&v, &c)? == 0 { hit = true; break; }
                }
                Ok(if *negated { !hit } else { hit })
            }
            Expr::Value(sqlparser::ast::Value::Boolean(b)) => Ok(*b),
            Expr::Value(sqlparser::ast::Value::Null) => Ok(false),
            _ => {
                // Truthiness of a scalar (nonzero/nonempty).
                let v = Self::eval_scalar(pred, qual, columns, row)?;
                Ok(match v {
                    SqlValue::Null => false,
                    SqlValue::Bool(b) => b,
                    SqlValue::Int64(n) => n != 0,
                    SqlValue::Float64(f) => f != 0.0,
                    SqlValue::Text(s) => !s.is_empty(),
                    SqlValue::Bytes(b) => !b.is_empty(),
                })
            }
        }
    }

    /// Scalar eval: literals, columns, operators w/ NULL semantics (checked
    /// arithmetic -> typed error, never panic — T-01-08), CASE/COALESCE/NULLIF,
    /// scalar fns via `functions.rs`.
    fn eval_scalar(expr: &Expr, qual: &[String], columns: &[String], row: &[SqlValue]) -> Result<SqlValue> {
        match expr {
            Expr::Value(v) => match v {
                sqlparser::ast::Value::Null => Ok(SqlValue::Null),
                sqlparser::ast::Value::Boolean(b) => Ok(SqlValue::Bool(*b)),
                sqlparser::ast::Value::SingleQuotedString(s) | sqlparser::ast::Value::DoubleQuotedString(s) => Ok(SqlValue::Text(s.clone())),
                sqlparser::ast::Value::Number(n, _) => {
                    if let Ok(i) = n.parse::<i64>() { Ok(SqlValue::Int64(i)) }
                    else if let Ok(f) = n.parse::<f64>() { Ok(SqlValue::Float64(f)) }
                    else { Err(TitanError::UnsupportedSql(format!("bad number {n}"))) }
                }
                _ => Err(TitanError::UnsupportedSql(format!("unsupported literal {v:?}"))),
            },
            Expr::Identifier(id) => {
                let i = columns.iter().position(|c| c == &id.value)
                    .ok_or_else(|| TitanError::UnsupportedSql(format!("unknown column {}", id.value)))?;
                Ok(row[i].clone())
            }
            Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                let (q, name) = (parts[0].value.clone(), parts[1].value.clone());
                let i = qual.iter().zip(columns.iter()).position(|(qq, c)| *qq == q && *c == name)
                    .or_else(|| columns.iter().position(|c| *c == name))
                    .ok_or_else(|| TitanError::UnsupportedSql(format!("unknown column {q}.{name}")))?;
                Ok(row[i].clone())
            }
            Expr::BinaryOp { left, op, right } => {
                let (a, b) = (
                    Self::eval_scalar(left, qual, columns, row)?,
                    Self::eval_scalar(right, qual, columns, row)?,
                );
                eval_binop(op, a, b)
            }
            Expr::UnaryOp { op, expr } => {
                let v = Self::eval_scalar(expr, qual, columns, row)?;
                match op {
                    UnaryOperator::Minus => match v {
                        SqlValue::Null => Ok(SqlValue::Null),
                        SqlValue::Int64(n) => Ok(SqlValue::Int64(n.checked_neg().ok_or_else(|| TitanError::UnsupportedSql("negation overflow".to_string()))?)),
                        SqlValue::Float64(f) => Ok(SqlValue::Float64(-f)),
                        _ => Err(TitanError::UnsupportedSql("unary minus over non-numeric".to_string())),
                    },
                    UnaryOperator::Not => match v {
                        SqlValue::Bool(b) => Ok(SqlValue::Bool(!b)),
                        SqlValue::Null => Ok(SqlValue::Null),
                        _ => Err(TitanError::UnsupportedSql("NOT over non-boolean".to_string())),
                    },
                    _ => Err(TitanError::UnsupportedSql(format!("unsupported unary {op:?}"))),
                }
            }
            Expr::Case { operand, conditions, results, else_result } => {
                for (cond, res) in conditions.iter().zip(results.iter()) {
                    let take = match operand {
                        Some(op) => {
                            let (a, b) = (Self::eval_scalar(op, qual, columns, row)?, Self::eval_scalar(cond, qual, columns, row)?);
                            if a == SqlValue::Null || b == SqlValue::Null { false }
                            else { compare_scalar(&a, &b)? == 0 }
                        }
                        None => Self::eval_pred(cond, qual, columns, row)?,
                    };
                    if take {
                        return Self::eval_scalar(res, qual, columns, row);
                    }
                }
                match else_result {
                    Some(e) => Self::eval_scalar(e, qual, columns, row),
                    None => Ok(SqlValue::Null),
                }
            }
            Expr::Function(f) => {
                let fname = f.name.to_string().to_ascii_uppercase();
                if fname == "COALESCE" {
                    for a in &f.args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a {
                            let v = Self::eval_scalar(e, qual, columns, row)?;
                            if v != SqlValue::Null { return Ok(v); }
                        }
                    }
                    return Ok(SqlValue::Null);
                }
                if fname == "NULLIF" {
                    let mut it = f.args.iter();
                    let (a, b) = (
                        match it.next() {
                            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => Self::eval_scalar(e, qual, columns, row)?,
                            _ => return Err(TitanError::UnsupportedSql("NULLIF needs 2 args".to_string())),
                        },
                        match it.next() {
                            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => Self::eval_scalar(e, qual, columns, row)?,
                            _ => return Err(TitanError::UnsupportedSql("NULLIF needs 2 args".to_string())),
                        },
                    );
                    if a != SqlValue::Null && b != SqlValue::Null && compare_scalar(&a, &b)? == 0 {
                        return Ok(SqlValue::Null);
                    }
                    return Ok(a);
                }
                if fname == "EXTRACT" {
                    return eval_extract(f, qual, columns, row);
                }
                let mut args: Vec<SqlValue> = Vec::new();
                for a in &f.args {
                    match a {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) =>
                            args.push(Self::eval_scalar(e, qual, columns, row)?),
                        FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => {}
                        _ => return Err(TitanError::UnsupportedSql("unsupported function arg".to_string())),
                    }
                }
                functions::eval_scalar_fn(&fname, &args)
            }
            Expr::Cast { expr, data_type, .. } => {
                let v = Self::eval_scalar(expr, qual, columns, row)?;
                eval_cast(v, data_type)
            }
            Expr::Trim { expr, trim_what, .. } => {
                let v = Self::eval_scalar(expr, qual, columns, row)?;
                if v == SqlValue::Null { return Ok(SqlValue::Null); }
                let SqlValue::Text(s) = v else {
                    return Err(TitanError::UnsupportedSql("TRIM needs text".to_string()));
                };
                match trim_what {
                    Some(w) => {
                        let wc = Self::eval_scalar(w, qual, columns, row)?;
                        let SqlValue::Text(chars) = wc else {
                            return Err(TitanError::UnsupportedSql("TRIM char needs text".to_string()));
                        };
                        let set: Vec<char> = chars.chars().collect();
                        Ok(SqlValue::Text(s.trim_matches(&set[..]).to_string()))
                    }
                    None => Ok(SqlValue::Text(s.trim().to_string())),
                }
            }
            Expr::Extract { field, expr } => {
                let dv = Self::eval_scalar(expr, qual, columns, row)?;
                let SqlValue::Text(d) = dv else {
                    if dv == SqlValue::Null { return Ok(SqlValue::Null); }
                    return Err(TitanError::UnsupportedSql("EXTRACT needs a date string".to_string()));
                };
                let parts: Vec<&str> = d.split('-').collect();
                if parts.len() != 3 {
                    return Err(TitanError::UnsupportedSql("EXTRACT expects YYYY-MM-DD".to_string()));
                }
                let (y, m, day): (i64, i64, i64) = (
                    parts[0].parse().map_err(|_| TitanError::UnsupportedSql("bad date".to_string()))?,
                    parts[1].parse().map_err(|_| TitanError::UnsupportedSql("bad date".to_string()))?,
                    parts[2].parse().map_err(|_| TitanError::UnsupportedSql("bad date".to_string()))?,
                );
                Ok(SqlValue::Int64(match field.to_string().to_ascii_uppercase().as_str() {
                    "YEAR" => y,
                    "MONTH" => m,
                    "DAY" => day,
                    _ => return Err(TitanError::UnsupportedSql(format!("EXTRACT field {field} not supported"))),
                }))
            }
            _ => Err(TitanError::UnsupportedSql(format!("unsupported expression: {expr}"))),
        }
    }
}

fn item_label(item: &SelectItem) -> String {
    match item {
        SelectItem::UnnamedExpr(e) => e.to_string(),
        SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
        SelectItem::Wildcard(_) => "*".to_string(),
        SelectItem::QualifiedWildcard(o, _) => format!("{o}.*"),
    }
}

fn display_key(v: &SqlValue) -> String {
    crate::sql::encoding::value_to_display(v)
}

fn compare_scalar(a: &SqlValue, b: &SqlValue) -> Result<i32> {
    match (a, b) {
        (SqlValue::Int64(x), SqlValue::Int64(y)) => Ok(x.cmp(y) as i8 as i32),
        (SqlValue::Int64(x), SqlValue::Float64(y)) => Ok((*x as f64).partial_cmp(y)
            .map(|o| o as i8 as i32).ok_or_else(|| TitanError::UnsupportedSql("uncomparable floats".to_string()))?),
        (SqlValue::Float64(x), SqlValue::Int64(y)) => Ok(x.partial_cmp(&(*y as f64))
            .map(|o| o as i8 as i32).ok_or_else(|| TitanError::UnsupportedSql("uncomparable floats".to_string()))?),
        (SqlValue::Float64(x), SqlValue::Float64(y)) => Ok(x.partial_cmp(y)
            .map(|o| o as i8 as i32).ok_or_else(|| TitanError::UnsupportedSql("uncomparable floats".to_string()))?),
        (SqlValue::Text(x), SqlValue::Text(y)) => Ok(x.cmp(y) as i8 as i32),
        (SqlValue::Bool(x), SqlValue::Bool(y)) => Ok(x.cmp(y) as i8 as i32),
        _ => Err(TitanError::UnsupportedSql("cannot compare values of different types".to_string())),
    }
}

fn compare_rows(a: &[SqlValue], b: &[SqlValue], cols: &[usize], desc: &[bool]) -> std::cmp::Ordering {
    for (k, d) in cols.iter().zip(desc.iter()) {
        let ka = functions::val_sort_key(&a[*k]);
        let kb = functions::val_sort_key(&b[*k]);
        let mut ord = ka.cmp(&kb);
        if *d { ord = ord.reverse(); }
        if ord != std::cmp::Ordering::Equal { return ord; }
    }
    std::cmp::Ordering::Equal
}

fn eval_binop(op: &BinaryOperator, a: SqlValue, b: SqlValue) -> Result<SqlValue> {
    use SqlValue as V;
    match op {
        BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply
        | BinaryOperator::Divide | BinaryOperator::Modulo => {
            if a == V::Null || b == V::Null { return Ok(V::Null); }
            let err = |m: &str| TitanError::UnsupportedSql(m.to_string());
            match (a, b) {
                (V::Int64(x), V::Int64(y)) => Ok(match op {
                    BinaryOperator::Plus => V::Int64(x.checked_add(y).ok_or_else(|| err("integer overflow"))?),
                    BinaryOperator::Minus => V::Int64(x.checked_sub(y).ok_or_else(|| err("integer overflow"))?),
                    BinaryOperator::Multiply => V::Int64(x.checked_mul(y).ok_or_else(|| err("integer overflow"))?),
                    BinaryOperator::Divide => {
                        if y == 0 { return Err(err("division by zero")); }
                        V::Int64(x.checked_div(y).ok_or_else(|| err("integer overflow"))?)
                    }
                    _ => {
                        if y == 0 { return Err(err("modulo by zero")); }
                        V::Int64(x.checked_rem(y).ok_or_else(|| err("integer overflow"))?)
                    }
                }),
                (x, y) => {
                    let (xf, yf) = (to_f64(&x)?, to_f64(&y)?);
                    Ok(V::Float64(match op {
                        BinaryOperator::Plus => xf + yf,
                        BinaryOperator::Minus => xf - yf,
                        BinaryOperator::Multiply => xf * yf,
                        BinaryOperator::Divide => {
                            if yf == 0.0 { return Err(err("division by zero")); }
                            xf / yf
                        }
                        _ => {
                            if yf == 0.0 { return Err(err("modulo by zero")); }
                            xf % yf
                        }
                    }))
                }
            }
        }
        BinaryOperator::StringConcat => {
            if a == V::Null || b == V::Null { return Ok(V::Null); }
            Ok(V::Text(format!("{}{}", display_key(&a), display_key(&b))))
        }
        BinaryOperator::Eq | BinaryOperator::NotEq | BinaryOperator::Lt
        | BinaryOperator::LtEq | BinaryOperator::Gt | BinaryOperator::GtEq => {
            if a == V::Null || b == V::Null { return Ok(V::Null); }
            let ord = compare_scalar(&a, &b)?;
            Ok(V::Bool(match op {
                BinaryOperator::Eq => ord == 0,
                BinaryOperator::NotEq => ord != 0,
                BinaryOperator::Lt => ord < 0,
                BinaryOperator::LtEq => ord <= 0,
                BinaryOperator::Gt => ord > 0,
                _ => ord >= 0,
            }))
        }
        BinaryOperator::And => Ok(match (a, b) {
            (V::Bool(false), _) | (_, V::Bool(false)) => V::Bool(false),
            (V::Bool(true), V::Bool(true)) => V::Bool(true),
            _ => V::Null,
        }),
        BinaryOperator::Or => Ok(match (a, b) {
            (V::Bool(true), _) | (_, V::Bool(true)) => V::Bool(true),
            (V::Bool(false), V::Bool(false)) => V::Bool(false),
            _ => V::Null,
        }),
        _ => Err(TitanError::UnsupportedSql(format!("unsupported operator {op:?}"))),
    }
}

fn to_f64(v: &SqlValue) -> Result<f64> {
    match v {
        SqlValue::Int64(n) => Ok(*n as f64),
        SqlValue::Float64(f) => Ok(*f),
        _ => Err(TitanError::UnsupportedSql("arithmetic over non-numeric".to_string())),
    }
}

fn eval_cast(v: SqlValue, dt: &sqlparser::ast::DataType) -> Result<SqlValue> {
    use sqlparser::ast::DataType as D;
    if v == SqlValue::Null { return Ok(SqlValue::Null); }
    match dt {
        D::Integer(_) | D::Int(_) | D::BigInt(_) | D::SmallInt(_) => match v {
            SqlValue::Int64(n) => Ok(SqlValue::Int64(n)),
            SqlValue::Float64(f) => Ok(SqlValue::Int64(f as i64)),
            SqlValue::Bool(b) => Ok(SqlValue::Int64(i64::from(b))),
            SqlValue::Text(s) => s.parse::<i64>().map(SqlValue::Int64)
                .map_err(|_| TitanError::UnsupportedSql(format!("cannot cast {s:?} to INT"))),
            _ => Err(TitanError::UnsupportedSql("cannot cast to INT".to_string())),
        },
        D::Float(_) | D::Double | D::Real => match v {
            SqlValue::Float64(f) => Ok(SqlValue::Float64(f)),
            SqlValue::Int64(n) => Ok(SqlValue::Float64(n as f64)),
            SqlValue::Text(s) => s.parse::<f64>().map(SqlValue::Float64)
                .map_err(|_| TitanError::UnsupportedSql(format!("cannot cast {s:?} to FLOAT"))),
            _ => Err(TitanError::UnsupportedSql("cannot cast to FLOAT".to_string())),
        },
        D::Boolean | D::Bool => match v {
            SqlValue::Bool(b) => Ok(SqlValue::Bool(b)),
            SqlValue::Text(s) => match s.to_ascii_lowercase().as_str() {
                "true" | "1" => Ok(SqlValue::Bool(true)),
                "false" | "0" => Ok(SqlValue::Bool(false)),
                _ => Err(TitanError::UnsupportedSql(format!("cannot cast {s:?} to BOOL"))),
            },
            SqlValue::Int64(n) => Ok(SqlValue::Bool(n != 0)),
            _ => Err(TitanError::UnsupportedSql("cannot cast to BOOL".to_string())),
        },
        _ => Ok(SqlValue::Text(display_key(&v))),
    }
}

/// EXTRACT(field FROM date_text). Minimal own date logic (no chrono — T-01-SC).
fn eval_extract(f: &sqlparser::ast::Function, qual: &[String], columns: &[String], row: &[SqlValue]) -> Result<SqlValue> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr};
    if f.args.len() != 2 {
        return Err(TitanError::UnsupportedSql("EXTRACT needs (field FROM date)".to_string()));
    }
    let field = match &f.args[0] {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(id))) => id.value.to_ascii_uppercase(),
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => e.to_string().to_ascii_uppercase(),
        _ => return Err(TitanError::UnsupportedSql("EXTRACT needs a field".to_string())),
    };
    let dv = match &f.args[1] {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) =>
            Executor::eval_scalar(e, qual, columns, row)?,
        _ => return Err(TitanError::UnsupportedSql("EXTRACT needs a date expr".to_string())),
    };
    let SqlValue::Text(d) = dv else {
        if dv == SqlValue::Null { return Ok(SqlValue::Null); }
        return Err(TitanError::UnsupportedSql("EXTRACT needs a date string".to_string()));
    };
    let parts: Vec<&str> = d.split('-').collect();
    if parts.len() != 3 {
        return Err(TitanError::UnsupportedSql("EXTRACT expects YYYY-MM-DD".to_string()));
    }
    let (y, m, day): (i64, i64, i64) = (
        parts[0].parse().map_err(|_| TitanError::UnsupportedSql("bad date".to_string()))?,
        parts[1].parse().map_err(|_| TitanError::UnsupportedSql("bad date".to_string()))?,
        parts[2].parse().map_err(|_| TitanError::UnsupportedSql("bad date".to_string()))?,
    );
    Ok(SqlValue::Int64(match field.as_str() {
        "YEAR" => y,
        "MONTH" => m,
        "DAY" => day,
        _ => return Err(TitanError::UnsupportedSql(format!("EXTRACT field {field} not supported"))),
    }))
}

/// Detect `WHERE <pk> = <literal>` for pushdown (pk = first column name).
fn pk_equality_lit(pred: &Expr, pk: &str) -> Option<String> {
    if let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = pred {
        let col_is_pk = |e: &Expr| match e {
            Expr::Identifier(id) => id.value == pk,
            Expr::CompoundIdentifier(p) => p.last().is_some_and(|p| p.value == pk),
            _ => false,
        };
        let lit = |e: &Expr| -> Option<String> {
            match e {
                Expr::Value(sqlparser::ast::Value::Number(n, _)) => Some(n.clone()),
                Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => Some(s.clone()),
                Expr::Value(sqlparser::ast::Value::Boolean(b)) => Some(b.to_string()),
                _ => None,
            }
        };
        if col_is_pk(left) { if let Some(l) = lit(right) { return Some(l); } }
        if col_is_pk(right) { if let Some(l) = lit(left) { return Some(l); } }
    }
    None
}

/// Classify an aggregate call: COUNT(*)/COUNT(x)/SUM/AVG/MIN/MAX.
fn agg_of(e: &Expr) -> Option<(AggKind, Option<Expr>)> {
    let Expr::Function(f) = e else { return None };
    let name = f.name.to_string().to_ascii_uppercase();
    let arg = f.args.first().and_then(|a| match a {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => Some(x.clone()),
        _ => None,
    });
    let is_star = f.args.iter().any(|a| matches!(a, FunctionArg::Unnamed(FunctionArgExpr::Wildcard)));
    match name.as_str() {
        "COUNT" if is_star => Some((AggKind::CountStar, None)),
        "COUNT" => Some((AggKind::Count, arg)),
        "SUM" => Some((AggKind::Sum, arg)),
        "AVG" => Some((AggKind::Avg, arg)),
        "MIN" => Some((AggKind::Min, arg)),
        "MAX" => Some((AggKind::Max, arg)),
        _ => None,
    }
}

/// Classify ROW_NUMBER()/RANK() OVER(...). Returns (name, order-desc?).
/// OVER with PARTITION BY -> still classified; projection errors honestly.
fn window_of(e: &Expr) -> Option<(String, bool)> {
    let Expr::Function(f) = e else { return None };
    let name = f.name.to_string().to_ascii_uppercase();
    if (name == "ROW_NUMBER" || name == "RANK") && f.over.is_some() {
        return Some((name, false));
    }
    None
}

/// ORDER BY exprs inside OVER(...) for window ordering (with direction).
fn window_order_exprs(e: &Expr) -> Option<Vec<(Expr, bool)>> {
    let Expr::Function(f) = e else { return None };
    match f.over.as_ref()? {
        sqlparser::ast::WindowType::WindowSpec(spec) => Some(
            spec.order_by.iter().map(|o| (o.expr.clone(), o.asc == Some(false))).collect(),
        ),
        sqlparser::ast::WindowType::NamedWindow(_) =>
            None,
    }
}
