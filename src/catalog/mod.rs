use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use crate::storage::page::PageId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataType {
    Integer,
    Text,
    Boolean,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub root_page_id: PageId, // B-Link Tree root for this table
}

pub struct Catalog {
    pub tables: HashMap<String, TableSchema>,
}

impl Catalog {
    pub fn new() -> Self {
        Catalog {
            tables: HashMap::new(),
        }
    }
}
