use serde::{Deserialize, Serialize};
use crate::error::Result;

pub const PAGE_SIZE: usize = 4096;

pub type PageId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PageType {
    Leaf,
    Interior,
    Overflow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageHeader {
    pub page_id: PageId,
    pub page_type: PageType,
    pub lsn: u64, // Log Sequence Number
    pub high_key: Option<Vec<u8>>, // B-Link high key
    pub right_link: Option<PageId>, // B-Link right link
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeContent {
    pub keys: Vec<Vec<u8>>,
    pub values: Vec<Vec<u8>>, // If leaf, actual values. If interior, PageIds (serialized).
}

#[derive(Debug, Clone)]
pub struct Page {
    pub header: PageHeader,
    pub content: NodeContent,
    pub dirty: bool,
}

impl Page {
    pub fn new(page_id: PageId, page_type: PageType) -> Self {
        Page {
            header: PageHeader {
                page_id,
                page_type,
                lsn: 0,
                high_key: None,
                right_link: None,
            },
            content: NodeContent {
                keys: Vec::new(),
                values: Vec::new(),
            },
            dirty: true,
        }
    }

    pub fn serialize(&self) -> Result<Vec<u8>> {
        let mut buffer = Vec::with_capacity(PAGE_SIZE);
        bincode::serialize_into(&mut buffer, &self.header)?;
        bincode::serialize_into(&mut buffer, &self.content)?;
        // Pad or truncate to ensure size fits - simplified here
        Ok(buffer)
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let mut cursor = std::io::Cursor::new(bytes);
        let header: PageHeader = bincode::deserialize_from(&mut cursor)?;
        let content: NodeContent = bincode::deserialize_from(&mut cursor)?;
        Ok(Page {
            header,
            content,
            dirty: false,
        })
    }
}
