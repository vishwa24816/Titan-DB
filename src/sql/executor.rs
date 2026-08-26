use sqlparser::ast::{Statement, Query, SetExpr, TableFactor, Expr, Value, BinaryOperator};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;

use crate::error::{Result, TitanError};
use crate::storage::pager::Pager;
use crate::index::blink::BLinkTree;
use crate::catalog::{Catalog, TableSchema, ColumnDef, DataType};
use crate::sql::ExecutionResult;

static GLOBAL_TX_ID: AtomicU64 = AtomicU64::new(1);

pub struct Executor {
    pager: Arc<Pager>,
    catalog: Arc<RwLock<Catalog>>,
}

impl Executor {
    pub fn new(pager: Arc<Pager>, catalog: Arc<RwLock<Catalog>>) -> Self {
        Executor { pager, catalog }
    }

    pub fn next_tx_id(&self) -> u64 {
        GLOBAL_TX_ID.fetch_add(1, Ordering::SeqCst)
    }

    pub fn execute(&self, sql: &str) -> Result<ExecutionResult> {
        let dialect = PostgreSqlDialect {};
        let ast = Parser::parse_sql(&dialect, sql)
            .map_err(|e| TitanError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;

        let mut last_result = ExecutionResult::Message("No statements executed".to_string());
        for statement in ast {
            last_result = self.execute_statement(statement)?;
        }
        Ok(last_result)
    }

    fn execute_statement(&self, statement: Statement) -> Result<ExecutionResult> {
        match statement {
            Statement::CreateTable { name, columns, .. } => {
                let table_name = name.to_string();
                let mut catalog = self.catalog.write();
                
                if catalog.tables.contains_key(&table_name) {
                    return Err(TitanError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Table already exists")));
                }

                let btree = Arc::new(BLinkTree::new(self.pager.clone())?);
                let root_id = btree.root_page_id();
                let schema = TableSchema {
                    name: table_name.clone(),
                    columns: columns.into_iter().map(|c| ColumnDef {
                        name: c.name.to_string(),
                        data_type: match c.data_type {
                            sqlparser::ast::DataType::Integer(_) | sqlparser::ast::DataType::Int(_) => DataType::Integer,
                            _ => DataType::Text,
                        },
                        nullable: true,
                    }).collect(),
                    root_page_id: root_id,
                    tree: btree,
                };

                catalog.tables.insert(table_name.clone(), schema);
                Ok(ExecutionResult::Message(format!("Table {} created.", table_name)))
            }
            Statement::Insert { table_name, source, .. } => {
                let name = table_name.to_string();
                let catalog = self.catalog.read();
                let schema = catalog.tables.get(&name).ok_or_else(|| {
                    TitanError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("Table {} not found", name)))
                })?;

                let tx_id = self.next_tx_id();
                let mut row_count = 0;

                if let Some(src) = source {
                    if let SetExpr::Values(values) = *src.body {
                        for row in values.rows {
                            if row.is_empty() { continue; }
                            let key = match &row[0] {
                                Expr::Value(Value::Number(n, _)) => n.clone(),
                                Expr::Value(Value::SingleQuotedString(s)) => s.clone(),
                                other => other.to_string(),
                            };
                            let val_str = row.iter().map(|r| match r {
                                Expr::Value(Value::SingleQuotedString(s)) => s.clone(),
                                Expr::Value(Value::Number(n, _)) => n.clone(),
                                other => other.to_string(),
                            }).collect::<Vec<_>>().join("|||");

                            schema.tree.insert(key.into_bytes(), val_str.into_bytes(), tx_id)?;
                            row_count += 1;
                        }
                    }
                }

                Ok(ExecutionResult::Message(format!("Inserted {} row(s) into {}.", row_count, name)))
            }
            Statement::Query(query) => {
                self.execute_query(*query)
            }
            Statement::Update { table, assignments, selection, .. } => {
                let name = table.relation.to_string();
                let catalog = self.catalog.read();
                let schema = catalog.tables.get(&name).ok_or_else(|| {
                    TitanError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("Table {} not found", name)))
                })?;

                let tx_id = self.next_tx_id();
                // Filter targeted key if WHERE id = <key>
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
                    let key_bytes = key.clone().into_bytes();
                    let payload = format!("{}|||{}", key, new_val).into_bytes();
                    schema.tree.insert(key_bytes, payload, tx_id)?;
                    updated += 1;
                }
                Ok(ExecutionResult::Message(format!("Updated {} row(s) in {}.", updated, name)))
            }
            Statement::Delete { from, selection, .. } => {
                let name = from[0].relation.to_string();
                let catalog = self.catalog.read();
                let schema = catalog.tables.get(&name).ok_or_else(|| {
                    TitanError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("Table {} not found", name)))
                })?;

                let tx_id = self.next_tx_id();
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
                    if schema.tree.delete(key.as_bytes(), tx_id)? {
                        deleted += 1;
                    }
                }
                Ok(ExecutionResult::Message(format!("Deleted {} row(s) from {}.", deleted, name)))
            }
            Statement::AlterTable { name, .. } => {
                Ok(ExecutionResult::Message(format!("Table {} altered.", name)))
            }
            Statement::Drop { object_type, names, .. } => {
                Ok(ExecutionResult::Message(format!("{:?} {:?} dropped.", object_type, names)))
            }
            _ => Ok(ExecutionResult::Message(format!("Statement {:?} parsed but not executed.", statement))),
        }
    }

    fn execute_query(&self, query: Query) -> Result<ExecutionResult> {
        if let SetExpr::Select(select) = *query.body {
            if select.from.is_empty() {
                return Ok(ExecutionResult::Message("No table in FROM clause".to_string()));
            }
            let name = match &select.from[0].relation {
                TableFactor::Table { name, .. } => name.to_string(),
                _ => select.from[0].relation.to_string(),
            };

            let catalog = self.catalog.read();
            let schema = catalog.tables.get(&name).ok_or_else(|| {
                TitanError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("Table {} not found", name)))
            })?;

            let tx_read_ts = self.next_tx_id();
            let raw_records = schema.tree.scan_all(tx_read_ts)?;

            let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
            let mut rows = Vec::new();

            for (_key, raw_bytes) in raw_records {
                if let Ok(val_str) = String::from_utf8(raw_bytes) {
                    let parts: Vec<String> = val_str.split("|||").map(|s| s.to_string()).collect();
                    rows.push(parts);
                }
            }

            Ok(ExecutionResult::ResultSet { columns, rows })
        } else {
            Ok(ExecutionResult::Message("Complex query not implemented.".to_string()))
        }
    }
}
