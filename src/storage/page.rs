use serde::{Deserialize, Serialize};
use crate::error::Result;

pub const PAGE_SIZE: usize = 65536;

pub type PageId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PageType {
    Leaf,
    Interior,
    Overflow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MvccRecord {
    pub tx_created: u64,
    pub tx_expired: Option<u64>, // Some(tx_id) if deleted/updated
    pub key: Vec<u8>,
    pub data: Vec<u8>,
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
    pub records: Vec<MvccRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageData {
    pub header: PageHeader,
    pub content: NodeContent,
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
                records: Vec::new(),
            },
            dirty: true,
        }
    }

    pub fn serialize(&self) -> Result<Vec<u8>> {
        let page_data = PageData {
            header: self.header.clone(),
            content: self.content.clone(),
        };
        let mut encoded = bincode::serialize(&page_data)?;
        if encoded.len() < PAGE_SIZE {
            encoded.resize(PAGE_SIZE, 0);
        }
        Ok(encoded)
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let page_data: PageData = bincode::deserialize(bytes)?;
        Ok(Page {
            header: page_data.header,
            content: page_data.content,
            dirty: false,
        })
    }
}
