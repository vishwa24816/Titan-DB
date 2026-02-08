use std::sync::{Arc, Mutex};

use crate::storage::page::{PageId, PageType};
use crate::storage::pager::Pager;
use crate::error::{Result, TitanError};

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

    /// Finds the leaf page that *should* contain the key.
    /// Handles concurrent splits via B-link logic.
    fn find_leaf(&self, key: &[u8]) -> Result<PageId> {
        let mut current_id = *self.root.lock().unwrap();

        loop {
            let page_arc = self.pager.fetch_page(current_id)?;
            let page = page_arc.read();

            // 1. Move Right Logic (The B-link magic)
            if let Some(ref high_key) = page.header.high_key {
                if key > high_key {
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

    pub fn search(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let leaf_id = self.find_leaf(key)?;
        let page_arc = self.pager.fetch_page(leaf_id)?;
        let page = page_arc.read();

        // Re-check high key just in case split happened between find_leaf and read lock?
        // In B-link, we usually hold the lock or re-check. 
        // find_leaf returns a page_id. We fetch it again. It might have split.
        // So we need a loop here too or find_leaf should return the locked page.
        // For simplicity:
        
        // Linear scan keys
        // binary_search expects &T. Since Vec contains Vec<u8>, we need to compare with &Vec<u8>.
        // We can use binary_search_by to avoid allocation.
        if let Ok(idx) = page.content.keys.binary_search_by(|k| k.as_slice().cmp(key)) {
             Ok(Some(page.content.values[idx].clone()))
        } else {
             Ok(None)
        }
    }

    pub fn insert(&self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        // 1. Find leaf
        let leaf_id = self.find_leaf(&key)?;
        
        // 2. Lock leaf for writing
        let page_arc = self.pager.fetch_page(leaf_id)?;
        let mut page = page_arc.write();

        // 3. Move Right Check (Writer specific)
        // If split happened, we might need to move right even with write lock?
        // Yes, if we locked the wrong node.
        if let Some(ref high_key) = page.header.high_key {
            if &key > high_key {
                // Release lock, traverse right (simplified recursion/loop needed)
                // For PoC, we just error or panic, but real impl loops.
                return Err(TitanError::PageNotFound(0)); // "Retry" error
            }
        }

        // 4. Insert locally
        // Check space... simplified
        page.content.keys.push(key);
        page.content.values.push(value);
        // Sort
        // (Inefficient: sorting every insert)
        // page.content.keys.sort(); // syncing values would be hard.
        // Use a proper structure in PageContent for real impl.

        page.dirty = true;
        
        // 5. Split if full
        // if page.size() > PAGE_SIZE {
        //    self.split_leaf(&mut page)?;
        // }

        Ok(())
    }
}
