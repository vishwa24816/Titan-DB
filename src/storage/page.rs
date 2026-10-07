use serde::{Deserialize, Serialize};
use crate::error::Result;

pub const PAGE_SIZE: usize = 65536;

pub const PAGE_MAGIC: u32 = 0x54495450; // "TITP"

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
        let body = bincode::serialize(&page_data)?;
        // Oversize bodies are REJECTED (never truncated): truncating would
        // corrupt rows and break the pager's RowTooLarge contract.
        if body.len() > PAGE_SIZE - 12 {
            return Err(crate::error::TitanError::RowTooLarge(body.len()));
        }
        // Frame: magic u32 + len u32 + crc32 u32 + content padded to PAGE_SIZE.
        // CRC covers the ENTIRE padded content (not just body) so a flipped
        // bit anywhere — including padding — is detected. Corrupt bytes ->
        // CorruptPage, never garbage.
        let mut content = body;
        content.resize(PAGE_SIZE - 12, 0);
        let mut encoded = Vec::with_capacity(PAGE_SIZE);
        encoded.extend_from_slice(&PAGE_MAGIC.to_le_bytes());
        encoded.extend_from_slice(&(content.len() as u32).to_le_bytes());
        encoded.extend_from_slice(&crate::storage::wal::crc32(&content).to_le_bytes());
        encoded.extend_from_slice(&content);
        Ok(encoded)
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        // All-zero page (fresh alloc never written, zero-filled tail): treat as blank Leaf.
        if bytes.iter().all(|&b| b == 0) {
            return Ok(Page {
                header: PageHeader {
                    page_id: 0,
                    page_type: PageType::Leaf,
                    lsn: 0,
                    high_key: None,
                    right_link: None,
                },
                content: NodeContent { records: Vec::new() },
                dirty: false,
            });
        }
        if bytes.len() < 12 {
            return Err(crate::error::TitanError::CorruptPage(0, "page frame too short".to_string()));
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().map_err(|_| {
            crate::error::TitanError::CorruptPage(0, "bad page magic".to_string())
        })?);
        if magic != PAGE_MAGIC {
            return Err(crate::error::TitanError::CorruptPage(0, "bad page magic".to_string()));
        }
        let len = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| {
            crate::error::TitanError::CorruptPage(0, "bad page len".to_string())
        })?) as usize;
        let expect = u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| {
            crate::error::TitanError::CorruptPage(0, "bad page crc".to_string())
        })?);
        if 12 + len > bytes.len() {
            return Err(crate::error::TitanError::CorruptPage(0, "torn page payload".to_string()));
        }
        // Serialized bodies always fit in a page (pager rejects oversize);
        // a larger len means corruption — reject before bincode allocates.
        if len > PAGE_SIZE {
            return Err(crate::error::TitanError::CorruptPage(0, "page body exceeds page size".to_string()));
        }
        if bytes.len() < PAGE_SIZE {
            return Err(crate::error::TitanError::CorruptPage(0, "short page frame".to_string()));
        }
        // CRC covers the full padded content region, so corruption in body
        // OR padding is detected.
        let region = &bytes[12..PAGE_SIZE];
        if crate::storage::wal::crc32(region) != expect {
            return Err(crate::error::TitanError::CorruptPage(0, "page CRC mismatch".to_string()));
        }
        let body = &bytes[12..12 + len];
        // Corrupt inner length prefixes can make bincode panic (capacity
        // overflow) instead of returning Err — convert panics to CorruptPage.
        // Never panic on attacker-controlled crash images (T-01-04).
        let owned = body.to_vec();
        let decoded = std::panic::catch_unwind(move || bincode::deserialize::<PageData>(&owned));
        let page_data: PageData = match decoded {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                return Err(crate::error::TitanError::CorruptPage(
                    0,
                    format!("page decode failed: {}", e),
                ))
            }
            Err(_) => {
                return Err(crate::error::TitanError::CorruptPage(
                    0,
                    "page decode panicked on corrupt bytes".to_string(),
                ))
            }
        };
        Ok(Page {
            header: page_data.header,
            content: page_data.content,
            dirty: false,
        })
    }
}
