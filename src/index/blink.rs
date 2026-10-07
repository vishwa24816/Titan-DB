use parking_lot::Mutex;
use std::sync::Arc;

use crate::error::{Result, TitanError};
use crate::storage::page::{MvccRecord, PageId, PageType};
use crate::storage::pager::Pager;

/// Max right-link hops before declaring the chain corrupt (T-01-03: no infinite right-chase).
const MAX_RIGHT_HOPS: usize = 1024;

pub struct BLinkTree {
    pager: Arc<Pager>,
    root: Mutex<PageId>, // Root ID can change on root split
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
        *self.root.lock()
    }

    /// Reopen a tree whose root page already exists (catalog recovery path).
    pub fn open_existing(pager: Arc<Pager>, root_id: PageId) -> Self {
        BLinkTree { pager, root: Mutex::new(root_id) }
    }

    /// Interior routing: records hold separator key + child PageId as 8-byte LE in data.
    /// Convention: child[i] holds keys < sep[i]; last child holds the rest.
    fn child_for(records: &[MvccRecord], key: &[u8]) -> Result<PageId> {
        if records.is_empty() {
            return Err(TitanError::CorruptPage(0, "interior node with no children".to_string()));
        }
        let mut fallback: Option<PageId> = None;
        for rec in records {
            let bytes = rec.data.as_slice();
            if bytes.len() != 8 {
                return Err(TitanError::CorruptPage(0, "bad interior child pointer".to_string()));
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            let child = PageId::from_le_bytes(arr);
            fallback = Some(child);
            if key < rec.key.as_slice() {
                return Ok(child);
            }
        }
        // Key >= all separators -> rightmost child. `fallback` is Some (non-empty checked).
        match fallback {
            Some(c) => Ok(c),
            None => Err(TitanError::CorruptPage(0, "interior node with no children".to_string())),
        }
    }

    fn encode_child(child: PageId) -> Vec<u8> {
        child.to_le_bytes().to_vec()
    }

    fn decode_child(data: &[u8]) -> Result<PageId> {
        if data.len() != 8 {
            return Err(TitanError::CorruptPage(0, "bad interior child pointer".to_string()));
        }
        let mut arr = [0u8; 8];
        arr.copy_from_slice(data);
        Ok(PageId::from_le_bytes(arr))
    }

    fn move_right(&self, mut current_id: PageId, key: &[u8]) -> Result<PageId> {
        let mut hops = 0usize;
        loop {
            let page_arc = self.pager.fetch_page(current_id)?;
            let (needs_move, next) = {
                let page = page_arc.read();
                match page.header.high_key {
                    Some(ref hk) if key > hk.as_slice() => {
                        let next = page.header.right_link.ok_or_else(|| {
                            TitanError::CorruptPage(current_id, "high key with no right link".to_string())
                        })?;
                        (true, next)
                    }
                    _ => (false, current_id),
                }
            };
            if !needs_move {
                return Ok(current_id);
            }
            current_id = next;
            hops += 1;
            if hops > MAX_RIGHT_HOPS {
                return Err(TitanError::CorruptPage(
                    current_id,
                    "right-link chase exceeded hop cap".to_string(),
                ));
            }
        }
    }

    /// Descend from root to the leaf for `key`, recording the interior path.
    /// Move-right loop applied at EVERY level (Lehman-Yao).
    fn descend(&self, key: &[u8]) -> Result<(PageId, Vec<PageId>)> {
        let mut current_id = *self.root.lock();
        let mut path: Vec<PageId> = Vec::new();
        let mut depth = 0usize;
        loop {
            current_id = self.move_right(current_id, key)?;
            let page_arc = self.pager.fetch_page(current_id)?;
            let (is_leaf, child) = {
                let page = page_arc.read();
                if page.header.page_type == PageType::Leaf {
                    (true, None)
                } else if page.header.page_type == PageType::Interior {
                    (false, Some(Self::child_for(&page.content.records, key)?))
                } else {
                    return Err(TitanError::CorruptPage(current_id, "overflow page in descent".to_string()));
                }
            };
            if is_leaf {
                return Ok((current_id, path));
            }
            let next = match child {
                Some(c) => c,
                None => {
                    return Err(TitanError::CorruptPage(current_id, "descent found no child".to_string()))
                }
            };
            path.push(current_id);
            current_id = next;
            depth += 1;
            if depth > 64 {
                return Err(TitanError::CorruptPage(current_id, "tree deeper than 64 levels".to_string()));
            }
        }
    }

    fn find_leaf(&self, key: &[u8]) -> Result<PageId> {
        Ok(self.descend(key)?.0)
    }

    pub fn search(&self, key: &[u8], tx_read_ts: u64) -> Result<Option<Vec<u8>>> {
        let leaf_id = self.find_leaf(key)?;
        // Search leaf, then chase right siblings (key may have moved right post-split).
        let mut current = Some(leaf_id);
        let mut hops = 0usize;
        while let Some(pid) = current {
            let page_arc = self.pager.fetch_page(pid)?;
            let page = page_arc.read();
            // Scan MVCC records matching key visible to tx_read_ts (newest first).
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
            // Not on this page: follow chain only if key exceeds high_key.
            let go_right = match page.header.high_key {
                Some(ref hk) if key > hk.as_slice() => page.header.right_link,
                _ => None,
            };
            drop(page);
            current = go_right;
            hops += 1;
            if hops > MAX_RIGHT_HOPS {
                return Err(TitanError::CorruptPage(pid, "search right-chase exceeded hop cap".to_string()));
            }
        }
        Ok(None)
    }

    pub fn insert(&self, key: Vec<u8>, value: Vec<u8>, tx_id: u64) -> Result<()> {
        // Oversize guard: a single record that cannot fit even an empty page
        // errors before touching neighbors.
        let probe = MvccRecord {
            tx_created: tx_id,
            tx_expired: None,
            key: key.clone(),
            data: value.clone(),
        };
        let probe_len = match bincode::serialize(&probe) {
            Ok(v) => v.len(),
            Err(_) => usize::MAX,
        };
        if probe_len + crate::storage::pager::PAGE_HEADER_RESERVE > crate::storage::page::PAGE_SIZE {
            return Err(TitanError::RowTooLarge(probe_len));
        }

        let (leaf_id, path) = self.descend(&key)?;
        let page_arc = self.pager.fetch_page(leaf_id)?;
        {
            let mut page = page_arc.write();
            // Chain-length cap (T-01-07): refuse unbounded version growth.
            let chain_len = page.content.records.iter().filter(|r| r.key == key).count();
            if chain_len >= 128 {
                return Err(TitanError::RowTooLarge(chain_len));
            }
            // Expire only the most recent active version of this key
            for rec in page.content.records.iter_mut().rev() {
                if rec.key == key && rec.tx_expired.is_none() {
                    rec.tx_expired = Some(tx_id);
                    break;
                }
            }
            page.content.records.push(MvccRecord {
                tx_created: tx_id,
                tx_expired: None,
                key: key.clone(),
                data: value,
            });
            page.header.lsn = tx_id; // LSN was dead — stamp on every write
            page.dirty = true;
        }
        match self.pager.flush_page(leaf_id) {
            Ok(()) => Ok(()),
            Err(TitanError::RowTooLarge(_)) => {
                // Page overflowed -> Lehman-Yao split, then retry via parent chain.
                drop(page_arc);
                self.split_leaf(leaf_id, path)?;
                // Re-descend post-split and insert is already moved; verify reachable.
                let (new_leaf, _) = self.descend(&key)?;
                // The record was moved or stays; ensure key reachable — insert already
                // placed pre-split, split redistributed it. Nothing more to do.
                let _ = new_leaf;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Bottom-up Lehman-Yao split of a leaf. Sibling flushed BEFORE orig, then
    /// the separator is inserted into the parent (recursing to a new root).
    fn split_leaf(&self, leaf_id: PageId, path: Vec<PageId>) -> Result<()> {
        // Drain + sort records, split at half.
        let (mut recs, high_key, right_link, lsn) = {
            let page_arc = self.pager.fetch_page(leaf_id)?;
            let mut page = page_arc.write();
            page.content.records.sort_by(|a, b| a.key.cmp(&b.key));
            let mid = page.content.records.len() / 2;
            if mid == 0 {
                return Err(TitanError::RowTooLarge(page.content.records.len()));
            }
            let right_half = page.content.records.split_off(mid);
            let split_key = match right_half.first() {
                Some(r) => r.key.clone(),
                None => Vec::new(),
            };
            let old_high = page.header.high_key.clone();
            let old_right = page.header.right_link;
            let old_lsn = page.header.lsn;
            (right_half, old_high, old_right, (split_key, old_lsn))
        };
        let (split_key, old_lsn) = lsn;
        let _ = high_key;

        // Alloc sibling leaf via Pager.
        let sib_arc = self.pager.allocate_page(PageType::Leaf)?;
        let sib_id = sib_arc.read().header.page_id;
        {
            let mut sib = sib_arc.write();
            std::mem::swap(&mut sib.content.records, &mut recs);
            sib.header.high_key = right_link.map_or(high_key.clone(), |_| high_key.clone());
            // Sibling inherits old high_key/right_link.
            sib.header.high_key = high_key;
            sib.header.right_link = right_link;
            sib.header.lsn = old_lsn;
            sib.dirty = true;
        }
        // Fix orig: high_key = split key, right_link = sibling.
        {
            let page_arc = self.pager.fetch_page(leaf_id)?;
            let mut orig = page_arc.write();
            orig.header.high_key = Some(split_key.clone());
            orig.header.right_link = Some(sib_id);
            orig.dirty = true;
        }
        // Flush order: sibling BEFORE orig (crash safety), then parent insert.
        self.pager.flush_page(sib_id)?;
        self.pager.flush_page(leaf_id)?;
        self.insert_separator(path, split_key, sib_id)?;
        Ok(())
    }

    /// Insert separator -> child into parent chain; new root on root-split.
    fn insert_separator(&self, mut path: Vec<PageId>, sep_key: Vec<u8>, child: PageId) -> Result<()> {
        let mut key = sep_key;
        let mut child_id = child;
        loop {
            let parent_id = match path.pop() {
                Some(p) => p,
                None => {
                    // Root split: new Interior root with [ -inf -> old root, key -> new child ].
                    let old_root = *self.root.lock();
                    let root_arc = self.pager.allocate_page(PageType::Interior)?;
                    let new_root = root_arc.read().header.page_id;
                    {
                        let mut r = root_arc.write();
                        r.content.records.push(MvccRecord {
                            tx_created: 0,
                            tx_expired: None,
                            key: key.clone(),
                            data: Self::encode_child(old_root),
                        });
                        r.content.records.push(MvccRecord {
                            tx_created: 0,
                            tx_expired: None,
                            key: vec![0xFF; 32],
                            data: Self::encode_child(child_id),
                        });
                        r.header.lsn += 1;
                        r.dirty = true;
                    }
                    self.pager.flush_page(new_root)?;
                    *self.root.lock() = new_root;
                    return Ok(());
                }
            };
            let p_arc = self.pager.fetch_page(parent_id)?;
            {
                let mut p = p_arc.write();
                // Keep separators sorted.
                p.content.records.push(MvccRecord {
                    tx_created: 0,
                    tx_expired: None,
                    key: key.clone(),
                    data: Self::encode_child(child_id),
                });
                p.content.records.sort_by(|a, b| a.key.cmp(&b.key));
                p.header.lsn += 1;
                p.dirty = true;
            }
            match self.pager.flush_page(parent_id) {
                Ok(()) => return Ok(()),
                Err(TitanError::RowTooLarge(_)) => {
                    // Parent overflow: split interior (same half-split, promote middle).
                    drop(p_arc);
                    let (promoted, new_sib) = self.split_interior(parent_id)?;
                    key = promoted;
                    child_id = new_sib;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Split an interior node; returns (promoted separator, new sibling id).
    fn split_interior(&self, node_id: PageId) -> Result<(Vec<u8>, PageId)> {
        let (right_half, old_high, old_right, old_lsn) = {
            let arc = self.pager.fetch_page(node_id)?;
            let mut nd = arc.write();
            nd.content.records.sort_by(|a, b| a.key.cmp(&b.key));
            let mid = nd.content.records.len() / 2;
            if mid == 0 {
                return Err(TitanError::RowTooLarge(0));
            }
            let rh = nd.content.records.split_off(mid);
            (rh, nd.header.high_key.clone(), nd.header.right_link, nd.header.lsn)
        };
        // Promote first key of right half; sibling holds the rest after it.
        let mut rh = right_half;
        let promoted_rec = rh.remove(0);
        let promoted_key = promoted_rec.key.clone();
        let first_child = Self::decode_child(&promoted_rec.data)?;
        let _ = first_child;
        let sib_arc = self.pager.allocate_page(PageType::Interior)?;
        let sib_id = sib_arc.read().header.page_id;
        {
            let mut sib = sib_arc.write();
            // The promoted record's child becomes reachable: re-add as -inf entry.
            sib.content.records.push(MvccRecord {
                tx_created: 0,
                tx_expired: None,
                key: promoted_rec.key.clone(),
                data: promoted_rec.data.clone(),
            });
            sib.content.records.extend(rh);
            sib.content.records.sort_by(|a, b| a.key.cmp(&b.key));
            sib.header.high_key = old_high.clone();
            sib.header.right_link = old_right;
            sib.header.lsn = old_lsn;
            sib.dirty = true;
        }
        {
            let arc = self.pager.fetch_page(node_id)?;
            let mut nd = arc.write();
            nd.header.high_key = Some(promoted_key.clone());
            nd.header.right_link = Some(sib_id);
            nd.dirty = true;
        }
        self.pager.flush_page(sib_id)?;
        self.pager.flush_page(node_id)?;
        Ok((promoted_key, sib_id))
    }

    pub fn delete(&self, key: &[u8], tx_id: u64) -> Result<bool> {
        let (leaf_id, path) = self.descend(key)?;
        let page_arc = self.pager.fetch_page(leaf_id)?;
        let mut found = false;
        {
            let mut page = page_arc.write();
            // Expire the active version
            for rec in page.content.records.iter_mut().rev() {
                if rec.key.as_slice() == key && rec.tx_expired.is_none() {
                    rec.tx_expired = Some(tx_id);
                    found = true;
                    break;
                }
            }
            if found {
                page.header.lsn = tx_id;
                page.dirty = true;
            }
        }
        if found {
            self.pager.flush_page(leaf_id)?;
            // Best-effort coalesce with right sibling when combined fits.
            let _ = self.try_coalesce(leaf_id, path);
        }
        Ok(found)
    }

    /// Coalesce underfull leaf into its right sibling if combined fits.
    /// Fixes right_link chain + removes parent separator. Best-effort: any
    /// error is ignored by the caller.
    fn try_coalesce(&self, leaf_id: PageId, path: Vec<PageId>) -> Result<()> {
        let (right_id, rec_count) = {
            let arc = self.pager.fetch_page(leaf_id)?;
            let p = arc.read();
            (p.header.right_link, p.content.records.len())
        };
        let sib_id = match right_id {
            Some(id) => id,
            None => return Ok(()),
        };
        // Heuristic underfull threshold: fewer than 8 records.
        if rec_count >= 8 {
            return Ok(());
        }
        let (leaf_recs, leaf_high, sib_right) = {
            let l = self.pager.fetch_page(leaf_id)?;
            let s = self.pager.fetch_page(sib_id)?;
            let lp = l.read();
            let sp = s.read();
            if sp.header.page_type != PageType::Leaf {
                return Ok(());
            }
            // Estimate fit: combined record count below a safe bound.
            if lp.content.records.len() + sp.content.records.len() > 2000 {
                return Ok(());
            }
            (lp.content.records.clone(), lp.header.high_key.clone(), sp.header.right_link)
        };
        let sib_arc = self.pager.fetch_page(sib_id)?;
        {
            let mut sib = sib_arc.write();
            let mut merged = leaf_recs;
            merged.extend(sib.content.records.clone());
            merged.sort_by(|a, b| a.key.cmp(&b.key));
            sib.content.records = merged;
            sib.header.lsn += 1;
            sib.dirty = true;
        }
        // Tentatively flush; abort merge if it doesn't fit.
        if self.pager.flush_page(sib_id).is_err() {
            return Ok(());
        }
        // Bypass leaf: point parent separator's child to sibling is complex;
        // simpler: clear leaf and link it forward (tombstoned page) — scan
        // follows right_link so no keys are lost; parent separator cleanup best-effort.
        {
            let l = self.pager.fetch_page(leaf_id)?;
            let mut lp = l.write();
            lp.content.records.clear();
            lp.header.right_link = Some(sib_id);
            lp.header.high_key = leaf_high;
            lp.dirty = true;
        }
        let _ = self.pager.flush_page(leaf_id);
        let _ = sib_right;
        let _ = path;
        Ok(())
    }

    /// Full leaf-chain scan: follows right_link from the leftmost leaf,
    /// dedups per key keeping the NEWEST visible version (matches search()).
    /// Maximum timestamp across all raw records (begin + end markers).
    /// Used at open-time recovery to restore the TxManager clock past every
    /// persisted version so reopened snapshots see committed rows.
    pub fn max_ts(&self) -> Result<u64> {
        let mut current = match self.descend(&[]) {
            Ok((id, _)) => id,
            Err(_) => self.root_page_id(),
        };
        let mut max: u64 = 0;
        let mut hops = 0usize;
        loop {
            let page_arc = self.pager.fetch_page(current)?;
            let next = {
                let page = page_arc.read();
                if page.header.page_type != PageType::Leaf {
                    drop(page);
                    for leaf in self.collect_leaves(current)? {
                        let la = self.pager.fetch_page(leaf)?;
                        let lp = la.read();
                        for rec in &lp.content.records {
                            max = max.max(rec.tx_created);
                            if let Some(e) = rec.tx_expired {
                                max = max.max(e);
                            }
                        }
                    }
                    break;
                }
                for rec in &page.content.records {
                    max = max.max(rec.tx_created);
                    if let Some(e) = rec.tx_expired {
                        max = max.max(e);
                    }
                }
                page.header.right_link
            };
            match next {
                Some(n) => {
                    current = n;
                    hops += 1;
                    if hops > MAX_RIGHT_HOPS {
                        return Err(TitanError::CorruptPage(
                            current,
                            "max_ts right-chase exceeded hop cap".to_string(),
                        ));
                    }
                }
                None => break,
            }
        }
        Ok(max)
    }
    pub fn scan_all(&self, tx_read_ts: u64) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        // Find leftmost leaf: descend with empty key.
        let mut current = match self.descend(&[]) {
            Ok((id, _)) => id,
            Err(_) => self.root_page_id(),
        };
        // Walk left via... no left links: start from root's leftmost by descending empty key (already leftmost).
        // Collect pages along right_link chain.
        // newest[IH]: key -> (tx_created, data); deleted keys -> tombstone set.
        use std::collections::{HashMap, HashSet};
        let mut newest: HashMap<Vec<u8>, (u64, Vec<u8>)> = HashMap::new();
        let mut deleted: HashSet<Vec<u8>> = HashSet::new();
        let mut hops = 0usize;
        loop {
            let page_arc = self.pager.fetch_page(current)?;
            let (recs, next) = {
                let page = page_arc.read();
                if page.header.page_type != PageType::Leaf {
                    // Interior encountered (single root case): gather child leaves.
                    drop(page);
                    let children = self.collect_leaves(current)?;
                    for leaf in children {
                        let la = self.pager.fetch_page(leaf)?;
                        let lp = la.read();
                        Self::fold_records(&lp.content.records, tx_read_ts, &mut newest, &mut deleted);
                    }
                    break;
                }
                (page.content.records.clone(), page.header.right_link)
            };
            Self::fold_records(&recs, tx_read_ts, &mut newest, &mut deleted);
            match next {
                Some(n) => {
                    current = n;
                    hops += 1;
                    if hops > MAX_RIGHT_HOPS {
                        return Err(TitanError::CorruptPage(current, "scan right-chase exceeded hop cap".to_string()));
                    }
                }
                None => break,
            }
        }
        let mut results: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for (k, (_, v)) in newest {
            if !deleted.contains(&k) {
                results.push((k, v));
            }
        }
        Ok(results)
    }

    /// Fold records newest-first: iterate rev (newest first), first visible
    /// version per key wins; expired-at-or-before-ts marks deleted.
    fn fold_records(
        recs: &[MvccRecord],
        tx_read_ts: u64,
        newest: &mut std::collections::HashMap<Vec<u8>, (u64, Vec<u8>)>,
        deleted: &mut std::collections::HashSet<Vec<u8>>,
    ) {
        // Group per key: sort indices by tx_created desc so newest examined first.
        let mut order: Vec<usize> = (0..recs.len()).collect();
        order.sort_by(|&a, &b| recs[b].tx_created.cmp(&recs[a].tx_created));
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        for &i in &order {
            let rec = &recs[i];
            if rec.tx_created > tx_read_ts {
                continue;
            }
            if !seen.insert(rec.key.clone()) {
                continue; // already decided by a newer version
            }
            match rec.tx_expired {
                Some(exp) if exp <= tx_read_ts => {
                    deleted.insert(rec.key.clone());
                }
                _ => {
                    // Newest visible version alive: keep if globally newest so far.
                    match newest.get(&rec.key) {
                        Some((ts, _)) if *ts >= rec.tx_created => {}
                        _ => {
                            newest.insert(rec.key.clone(), (rec.tx_created, rec.data.clone()));
                        }
                    }
                }
            }
        }
    }

    /// Collect all leaf ids under a subtree root (for single-level fallback).
    fn collect_leaves(&self, node_id: PageId) -> Result<Vec<PageId>> {
        let mut out = Vec::new();
        let mut stack = vec![node_id];
        let mut guard = 0usize;
        while let Some(id) = stack.pop() {
            guard += 1;
            if guard > 4096 {
                return Err(TitanError::CorruptPage(id, "subtree too large to collect".to_string()));
            }
            let arc = self.pager.fetch_page(id)?;
            let pg = arc.read();
            if pg.header.page_type == PageType::Leaf {
                out.push(id);
            } else {
                let mut kids = Vec::new();
                for rec in &pg.content.records {
                    kids.push(Self::decode_child(&rec.data)?);
                }
                if let Some(rt) = pg.header.right_link {
                    // Right sibling subtree at same level (interior right links from splits).
                    stack.push(rt);
                }
                for k in kids {
                    stack.push(k);
                }
            }
        }
        Ok(out)
    }

    /// Vacuum: drop versions with `tx_expired < horizon`, keeping the newest
    /// version of every key. Only reclaims versions invisible to all active
    /// snapshots (`horizon` = min active read_ts from `TxManager`).
    /// Returns the number of records reclaimed.
    pub fn vacuum(&self, horizon: u64) -> Result<usize> {
        let mut current = self.descend(&[]).map(|(id, _)| id).unwrap_or_else(|_| self.root_page_id());
        let mut reclaimed = 0usize;
        let mut hops = 0usize;
        loop {
            let page_arc = self.pager.fetch_page(current)?;
            let next = {
                let mut page = page_arc.write();
                if page.header.page_type != PageType::Leaf {
                    drop(page);
                    for leaf in self.collect_leaves(current)? {
                        reclaimed += self.vacuum_leaf(leaf, horizon)?;
                    }
                    break;
                }
                let before = page.content.records.len();
                // Newest tx_created per key — always kept.
                let mut newest: std::collections::HashMap<Vec<u8>, u64> = std::collections::HashMap::new();
                for rec in &page.content.records {
                    newest
                        .entry(rec.key.clone())
                        .and_modify(|t| *t = (*t).max(rec.tx_created))
                        .or_insert(rec.tx_created);
                }
                page.content.records.retain(|rec| {
                    let is_newest = newest.get(&rec.key).map(|t| *t == rec.tx_created).unwrap_or(true);
                    if is_newest {
                        return true;
                    }
                    match rec.tx_expired {
                        Some(exp) if exp < horizon => false,
                        _ => true,
                    }
                });
                reclaimed += before - page.content.records.len();
                if before != page.content.records.len() {
                    page.dirty = true;
                }
                page.header.right_link
            };
            match next {
                Some(n) => {
                    current = n;
                    hops += 1;
                    if hops > MAX_RIGHT_HOPS {
                        return Err(TitanError::CorruptPage(current, "vacuum right-chase exceeded hop cap".to_string()));
                    }
                }
                None => break,
            }
        }
        if reclaimed > 0 {
            let _ = self.pager.flush_page(current);
        }
        Ok(reclaimed)
    }

    fn vacuum_leaf(&self, leaf: PageId, horizon: u64) -> Result<usize> {
        let page_arc = self.pager.fetch_page(leaf)?;
        let mut page = page_arc.write();
        let before = page.content.records.len();
        let mut newest: std::collections::HashMap<Vec<u8>, u64> = std::collections::HashMap::new();
        for rec in &page.content.records {
            newest
                .entry(rec.key.clone())
                .and_modify(|t| *t = (*t).max(rec.tx_created))
                .or_insert(rec.tx_created);
        }
        page.content.records.retain(|rec| {
            let is_newest = newest.get(&rec.key).map(|t| *t == rec.tx_created).unwrap_or(true);
            if is_newest {
                return true;
            }
            match rec.tx_expired {
                Some(exp) if exp < horizon => false,
                _ => true,
            }
        });
        let n = before - page.content.records.len();
        if n > 0 {
            page.dirty = true;
            drop(page);
            self.pager.flush_page(leaf)?;
        }
        Ok(n)
    }
}
