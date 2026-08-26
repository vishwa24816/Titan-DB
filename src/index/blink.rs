use std::sync::{Arc, Mutex};

use crate::storage::page::{PageId, PageType};
use crate::storage::pager::Pager;
use crate::error::Result;

pub struct BLinkTree {
    pager: Arc<Pager>,
    root: Mutex<PageId>, // Root ID can change, though usually fixed in disk-based
}

impl BLinkTree {
    pub fn new(pager: Arc<Pager>) -> Result<Self> {
        let root_page = pager.allocate_page(PageType::Leaf)?;
        let root_id = root_page.read().header.page_id;
        Ok(BLinkTree {
            pager,
            root: Mutex::new(root_id),
        })
    }

    pub fn root_page_id(&self) -> PageId {
        *self.root.lock().unwrap()
    }

    /// Finds the leaf page that *should* contain the key.
    /// Handles concurrent splits via B-link logic.
    fn find_leaf(&self, key: &[u8]) -> Result<PageId> {
        let mut current_id = *self.root.lock().unwrap();

        loop {
            let page_arc = self.pager.fetch_page(current_id)?;
            let page = page_arc.read();

            // 1. Move Right Logic (The B-link magic)
            if let Some(ref high_key) = page.header.high_key {
                if key > high_key.as_slice() {
                    let next_id = page.header.right_link.expect("High key exists but no right link");
                    current_id = next_id;
                    continue; // Re-fetch new node, release lock on old
                }
            }

            // 2. Leaf check
            if page.header.page_type == PageType::Leaf {
                return Ok(current_id);
            }

            // 3. Interior Node Search: Find child pointer
            // Simple linear scan for PoC
            let _child_id = 0; // Default or error
            // Assuming keys are sorted
            // keys: [k1, k2, k3]
            // children: [p0, p1, p2, p3] (usually k+1 children)
            // But for simplicity in Page struct, let's assume keys[i] >= child[i] max?
            // Standard B+Tree: keys separating children.
            // child[i] < key[i] <= child[i+1]
            // Let's assume content.values stores PageIds as 8-byte LE.
            
            // This part requires rigorous encoding which we skipped in Page struct for brevity.
            // Let's assume a simple key-value scan for now.
            // Since this is PoC, we might stub interior navigation.
            // For now, let's just return current_id if it's leaf (root is leaf initially).
            
            // If we are here, it means we have interior node logic to implement.
            // Let's implement a dummy descent to show structure:
            // current_id = read_child_id(&page, key);
             break; // TODO: Implement interior search
        }
        Ok(current_id) 
    }

    pub fn search(&self, key: &[u8], tx_read_ts: u64) -> Result<Option<Vec<u8>>> {
        let leaf_id = self.find_leaf(key)?;
        let page_arc = self.pager.fetch_page(leaf_id)?;
        let page = page_arc.read();

        // Scan MVCC records matching key visible to tx_read_ts
        for rec in page.content.records.iter().rev() {
            if rec.key.as_slice() == key {
                if rec.tx_created <= tx_read_ts {
                    if let Some(exp) = rec.tx_expired {
                        if exp <= tx_read_ts {
                            return Ok(None); // Deleted before our read snapshot
                        }
                    }
                    return Ok(Some(rec.data.clone()));
                }
            }
        }
        Ok(None)
    }

    pub fn insert(&self, key: Vec<u8>, value: Vec<u8>, tx_id: u64) -> Result<()> {
        let leaf_id = self.find_leaf(&key)?;
        let page_arc = self.pager.fetch_page(leaf_id)?;
        let mut page = page_arc.write();

        // Expire only the most recent active version of this key
        for rec in page.content.records.iter_mut().rev() {
            if rec.key == key && rec.tx_expired.is_none() {
                rec.tx_expired = Some(tx_id);
                break;
            }
        }

        page.content.records.push(crate::storage::page::MvccRecord {
            tx_created: tx_id,
            tx_expired: None,
            key,
            data: value,
        });

        page.dirty = true;
        drop(page);
        self.pager.flush_page(leaf_id)?;
        Ok(())
    }

    pub fn delete(&self, key: &[u8], tx_id: u64) -> Result<bool> {
        let leaf_id = self.find_leaf(key)?;
        let page_arc = self.pager.fetch_page(leaf_id)?;
        let mut page = page_arc.write();

        let mut found = false;
        // Expire the active version
        for rec in page.content.records.iter_mut().rev() {
            if rec.key.as_slice() == key && rec.tx_expired.is_none() {
                rec.tx_expired = Some(tx_id);
                found = true;
                break;
            }
        }

        if found {
            page.dirty = true;
            drop(page);
            self.pager.flush_page(leaf_id)?;
        }
        Ok(found)
    }

    pub fn scan_all(&self, tx_read_ts: u64) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let leaf_id = *self.root.lock().unwrap();
        let page_arc = self.pager.fetch_page(leaf_id)?;
        let page = page_arc.read();

        // Group by key and pick the visible version at tx_read_ts
        let mut map: std::collections::HashMap<Vec<u8>, Option<Vec<u8>>> = std::collections::HashMap::new();
        for rec in &page.content.records {
            if rec.tx_created <= tx_read_ts {
                let is_alive = match rec.tx_expired {
                    Some(exp) => exp > tx_read_ts,
                    None => true,
                };
                if is_alive {
                    map.insert(rec.key.clone(), Some(rec.data.clone()));
                } else {
                    // Deleted or superseded at or before tx_read_ts
                    if let Some(entry) = map.get_mut(&rec.key) {
                        *entry = None;
                    }
                }
            }
        }

        let mut results = Vec::new();
        for (k, v_opt) in map {
            if let Some(v) = v_opt {
                results.push((k, v));
            }
        }
        Ok(results)
    }
}
