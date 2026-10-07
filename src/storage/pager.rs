use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use crate::error::{Result, TitanError};
use crate::storage::page::{Page, PageId, PAGE_SIZE};

const SHARD_COUNT: usize = 16;

/// Reserve for the bincode page header; payloads beyond this are rejected
/// with RowTooLarge instead of being truncated into neighbors.
pub const PAGE_HEADER_RESERVE: usize = 512;

struct Shard {
    pages: HashMap<PageId, Arc<RwLock<Page>>>,
}

pub struct Pager {
    file: Mutex<File>,
    path: std::path::PathBuf,
    /// Serializes seek+read / seek+write syscall pairs (Windows cursor
    /// semantics). Wave-3-upgradeable shim: replace with pread/pwrite later.
    io_lock: Mutex<()>,
    shards: Vec<RwLock<Shard>>,
    total_pages: Mutex<PageId>,
}

impl Pager {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path_buf)?;

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
            path: path_buf,
            io_lock: Mutex::new(()),
            shards,
            total_pages: Mutex::new(total_pages),
        })
    }

    fn get_shard(&self, page_id: PageId) -> &RwLock<Shard> {
        &self.shards[(page_id as usize) % SHARD_COUNT]
    }

    /// Filesystem path of the backing .db file (sidecars derive from it).
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Durability barrier: sync data file to storage.
    pub fn sync_all(&self) -> Result<()> {
        self.file.lock().sync_all()?;
        Ok(())
    }

    /// Flush every cached dirty page, then sync. Used by checkpoint.
    pub fn flush_all(&self) -> Result<()> {
        let ids: Vec<PageId> = self
            .shards
            .iter()
            .flat_map(|s| s.read().pages.keys().cloned().collect::<Vec<_>>())
            .collect();
        for id in ids {
            self.flush_page(id)?;
        }
        self.sync_all()
    }

    /// Guard a raw payload against overflowing its page.
    pub fn check_payload_fits(len: usize) -> Result<()> {
        if len > PAGE_SIZE - PAGE_HEADER_RESERVE {
            return Err(TitanError::RowTooLarge(len));
        }
        Ok(())
    }

    fn read_page_from_disk(&self, page_id: PageId) -> Result<Page> {
        let _io = self.io_lock.lock();
        let mut file = self.file.lock();
        let offset = page_id * PAGE_SIZE as u64;
        // Fresh DB (len == 0) or never-written tail: return zeroed page
        // instead of UnexpectedEof.
        let file_len = file.metadata()?.len();
        if file_len == 0 || offset >= file_len {
            return Ok(Page::new(page_id, crate::storage::page::PageType::Leaf));
        }
        file.seek(SeekFrom::Start(offset))?;

        let mut buffer = vec![0u8; PAGE_SIZE];
        // Short read at EOF tail (file not page-aligned): zero-fill rest.
        let mut total = 0;
        while total < PAGE_SIZE {
            match file.read(&mut buffer[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) => return Err(e.into()),
            }
        }
        if total == 0 {
            return Ok(Page::new(page_id, crate::storage::page::PageType::Leaf));
        }

        let page = Page::deserialize(&buffer).map_err(|_| {
            TitanError::CorruptPage(page_id, "failed to decode page bytes".to_string())
        })?;
        Ok(page)
    }

    /// Cache-hit reads take NO file or io_lock — only the shard read
    /// lock (see early return below). Misses do positional seek+read
    /// under io_lock (Windows cursor safety). Upgrade path: per-thread
    /// file handles (try_clone) to drop io_lock; deferred — io_lock
    /// contention is negligible while the cache-hit fast path avoids it.
    pub fn fetch_page(&self, page_id: PageId) -> Result<Arc<RwLock<Page>>> {
        {
            let shard_lock = self.get_shard(page_id).read();
            if let Some(page) = shard_lock.pages.get(&page_id) {
                return Ok(page.clone());
            }
        }

        let page = self.read_page_from_disk(page_id)?;
        let page_arc = Arc::new(RwLock::new(page));

        // Double-checked insert: another thread may have won the race
        // while we were doing I/O — return the existing entry if so.
        let mut shard_write = self.get_shard(page_id).write();
        if let Some(existing) = shard_write.pages.get(&page_id) {
            return Ok(existing.clone());
        }
        shard_write.pages.insert(page_id, page_arc.clone());

        Ok(page_arc)
    }

    pub fn allocate_page(
        &self,
        page_type: crate::storage::page::PageType,
    ) -> Result<Arc<RwLock<Page>>> {
        let page_id = {
            let mut total_pages = self.total_pages.lock();
            let page_id = *total_pages;
            *total_pages += 1;
            page_id
        };

        let page = Page::new(page_id, page_type);
        let page_arc = Arc::new(RwLock::new(page));

        {
            let mut shard_write = self.get_shard(page_id).write();
            shard_write.pages.insert(page_id, page_arc.clone());
        }

        self.flush_page(page_id)?;
        Ok(page_arc)
    }

    pub fn flush_page(&self, page_id: PageId) -> Result<()> {
        let data = {
            let shard_read = self.get_shard(page_id).read();
            let page_lock = shard_read
                .pages
                .get(&page_id)
                .ok_or(TitanError::PageNotFound(page_id))?;
            let page = page_lock.read();
            if !page.dirty {
                return Ok(());
            }
            page.serialize()?
        };

        // Oversize guard: serialized page must fit; RowTooLarge instead
        // of silent truncation into neighbor pages.
        if data.len() > PAGE_SIZE {
            return Err(TitanError::RowTooLarge(data.len()));
        }
        let mut final_data = data;
        if final_data.len() < PAGE_SIZE {
            final_data.resize(PAGE_SIZE, 0);
        }

        {
            let _io = self.io_lock.lock();
            let mut file = self.file.lock();
            file.seek(SeekFrom::Start(page_id * PAGE_SIZE as u64))?;
            file.write_all(&final_data)?;
            file.flush()?;
        }
        {
            let shard_read = self.get_shard(page_id).read();
            if let Some(page_lock) = shard_read.pages.get(&page_id) {
                page_lock.write().dirty = false;
            }
        }
        Ok(())
    }
}
