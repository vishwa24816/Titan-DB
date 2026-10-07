use std::sync::Arc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use crate::storage::page::PageId;
use crate::index::blink::BLinkTree;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataType {
    Integer,
    Float,
    Text,
    Boolean,
    Bytes,
    Date,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub root_page_id: PageId,
    pub tree: Arc<BLinkTree>,
}

/// Persisted catalog entry (sidecar `<db>.catalog`): schema + root page + LSN.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTable {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub root_page_id: PageId,
    pub lsn: u64,
}

/// Sidecar path for a .db file.
pub fn catalog_path(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".catalog");
    std::path::PathBuf::from(s)
}

/// WAL path for a .db file.
pub fn wal_path(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".wal");
    std::path::PathBuf::from(s)
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

    /// Write every table schema to the sidecar + sync (called on CREATE/DROP + checkpoint).
    pub fn persist(&self, db_path: &std::path::Path, lsn: u64) -> crate::error::Result<()> {
        let entries: Vec<PersistedTable> = self
            .tables
            .values()
            .map(|t| PersistedTable {
                name: t.name.clone(),
                columns: t.columns.clone(),
                root_page_id: t.root_page_id,
                lsn,
            })
            .collect();
        let bytes = bincode::serialize(&entries)?;
        let path = catalog_path(db_path);
        std::fs::write(&path, &bytes)?;
        // Sync parent data via file sync on the sidecar itself (read+write
        // handle: Windows FlushFileBuffers needs GENERIC_WRITE).
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
        f.sync_all()?;
        Ok(())
    }

    /// Load sidecar entries. Missing file -> CatalogMissing (callers map fresh-DB to empty).
    pub fn load_entries(db_path: &std::path::Path) -> crate::error::Result<Vec<PersistedTable>> {
        let path = catalog_path(db_path);
        if !path.exists() {
            return Err(crate::error::TitanError::CatalogMissing(format!(
                "no catalog sidecar for {}",
                db_path.display()
            )));
        }
        let bytes = std::fs::read(&path)?;
        let entries: Vec<PersistedTable> = bincode::deserialize(&bytes).map_err(|_| {
            crate::error::TitanError::CatalogMissing("catalog sidecar corrupt".to_string())
        })?;
        Ok(entries)
    }
}
