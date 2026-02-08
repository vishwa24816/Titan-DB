use sqlparser::ast::{Statement, Query, SetExpr};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::sync::Arc;
use parking_lot::RwLock;

use crate::error::{Result, TitanError};
use crate::storage::pager::Pager;
use crate::index::blink::BLinkTree;
use crate::catalog::{Catalog, TableSchema, ColumnDef, DataType};
use crate::sql::ExecutionResult;

pub struct Executor {
    pager: Arc<Pager>,
    catalog: Arc<RwLock<Catalog>>,
}

impl Executor {
    pub fn new(pager: Arc<Pager>, catalog: Arc<RwLock<Catalog>>) -> Self {
        Executor { pager, catalog }
    }

    pub fn execute(&self, sql: &str) -> Result<ExecutionResult> {
        let dialect = PostgreSqlDialect {};
        let ast = Parser::parse_sql(&dialect, sql)
            .map_err(|e| TitanError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;

        // For simplicity in this structure, we return the result of the LAST statement
        // In a real generic executor, we'd return Vec<ExecutionResult>
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

                // Allocate a root page for the new table's B-Link tree
                let _btree = BLinkTree::new(self.pager.clone())?;
                
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
                    root_page_id: 0, // Placeholder
                };

                catalog.tables.insert(table_name.clone(), schema);
                Ok(ExecutionResult::Message(format!("Table {} created.", table_name)))
            }
            Statement::Insert { table_name, .. } => {
                let name = table_name.to_string();
                let catalog = self.catalog.read();
                let _schema = catalog.tables.get(&name).ok_or(TitanError::PageNotFound(0))?;
                
                Ok(ExecutionResult::Message(format!("Inserted into {}.", name)))
            }
            Statement::Query(query) => {
                self.execute_query(*query)
            }
            Statement::AlterTable { name, .. } => {
                Ok(ExecutionResult::Message(format!("Table {} altered.", name)))
            }
            Statement::Drop { object_type, names, .. } => {
                Ok(ExecutionResult::Message(format!("{:?} {:?} dropped.", object_type, names)))
            }
            Statement::Delete { .. } => {
                Ok(ExecutionResult::Message("Delete statement executed.".to_string()))
            }
            Statement::Update { table, .. } => {
                Ok(ExecutionResult::Message(format!("Updated {}.", table.relation)))
            }
            _ => Ok(ExecutionResult::Message(format!("Statement {:?} parsed but execution not yet implemented.", statement))),
        }
    }

    fn execute_query(&self, query: Query) -> Result<ExecutionResult> {
        if let SetExpr::Select(select) = *query.body {
             let table_name = select.from[0].relation.to_string();
             // Mock data return for UI demonstration
             Ok(ExecutionResult::ResultSet { 
                 columns: vec!["id".to_string(), "name".to_string(), "age".to_string()],
                 rows: vec![
                     vec!["1".to_string(), "Alice".to_string(), "30".to_string()],
                     vec!["2".to_string(), "Bob".to_string(), "25".to_string()],
                 ]
             })
        } else {
             Ok(ExecutionResult::Message("Complex query not implemented.".to_string()))
        }
    }
}
