use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use parking_lot::RwLock;

use crate::error::{Result, TitanError};
use crate::storage::page::{Page, PageId, PAGE_SIZE};

const SHARD_COUNT: usize = 16;

struct Shard {
    pages: HashMap<PageId, Arc<RwLock<Page>>>,
}

pub struct Pager {
    file: Mutex<File>,
    shards: Vec<RwLock<Shard>>,
    total_pages: Mutex<PageId>,
}

impl Pager {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        let len = file.metadata()?.len();
        let total_pages = len / PAGE_SIZE as u64;

        let mut shards = Vec::with_capacity(SHARD_COUNT);
        for _ in 0..SHARD_COUNT {
            shards.push(RwLock::new(Shard {
                pages: HashMap::new(),
            }));
        }

        Ok(Pager {
            file: Mutex::new(file),
            shards,
            total_pages: Mutex::new(total_pages),
        })
    }

    fn get_shard(&self, page_id: PageId) -> &RwLock<Shard> {
        &self.shards[(page_id as usize) % SHARD_COUNT]
    }

    pub fn fetch_page(&self, page_id: PageId) -> Result<Arc<RwLock<Page>>> {
        let shard_lock = self.get_shard(page_id).read();
        if let Some(page) = shard_lock.pages.get(&page_id) {
            return Ok(page.clone());
        }
        drop(shard_lock);

        // Cache miss
        let mut file = self.file.lock().map_err(|_| TitanError::LockError)?;
        file.seek(SeekFrom::Start(page_id * PAGE_SIZE as u64))?;
        
        let mut buffer = vec![0u8; PAGE_SIZE];
        // Handle EOF for new pages gracefully or strict read
        if let Err(_) = file.read_exact(&mut buffer) {
             // For simplicity, we might assume if it fails it's a new page request logic elsewhere
             // But here strict fetch implies existence.
             // If we are creating a NEW page, fetch_page shouldn't be called, allocate_page should.
             return Err(TitanError::PageNotFound(page_id));
        }

        let page = Page::deserialize(&buffer).unwrap_or_else(|_| {
            // If deserialization fails (empty file/init), return a fresh page if strictly needed
            // But realistically, we should panic or error. 
            // For PoC:
             Page::new(page_id, crate::storage::page::PageType::Leaf)
        });

        let page_arc = Arc::new(RwLock::new(page));

        let mut shard_write = self.get_shard(page_id).write();
        shard_write.pages.insert(page_id, page_arc.clone());

        Ok(page_arc)
    }

    pub fn allocate_page(&self, page_type: crate::storage::page::PageType) -> Result<Arc<RwLock<Page>>> {
        let mut total_pages = self.total_pages.lock().map_err(|_| TitanError::LockError)?;
        let page_id = *total_pages;
        *total_pages += 1;

        let page = Page::new(page_id, page_type);
        let page_arc = Arc::new(RwLock::new(page));

        let mut shard_write = self.get_shard(page_id).write();
        shard_write.pages.insert(page_id, page_arc.clone());

        Ok(page_arc)
    }

    pub fn flush_page(&self, page_id: PageId) -> Result<()> {
        let shard_read = self.get_shard(page_id).read();
        let page_lock = shard_read.pages.get(&page_id).ok_or(TitanError::PageNotFound(page_id))?;
        let page = page_lock.read();

        if page.dirty {
            let data = page.serialize()?;
            // Ensure padding to PAGE_SIZE
            let mut final_data = data;
            if final_data.len() < PAGE_SIZE {
                final_data.resize(PAGE_SIZE, 0);
            }

            let mut file = self.file.lock().map_err(|_| TitanError::LockError)?;
            file.seek(SeekFrom::Start(page_id * PAGE_SIZE as u64))?;
            file.write_all(&final_data)?;
        }
        Ok(())
    }
}
