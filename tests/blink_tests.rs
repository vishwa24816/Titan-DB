//! D-01/D-02 fail-first contracts: B-link splits, merges, scan visibility.
//! These tests SHOULD FAIL until Lehman-Yao descent/split/merge lands.

use std::sync::Arc;
use titan_db::index::blink::BLinkTree;
use titan_db::storage::pager::Pager;

fn test_dir() -> std::path::PathBuf {
    let base = std::env::var("TITAN_TEST_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let dir = std::path::PathBuf::from(base).join(format!("titan_blink_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn open_tree(name: &str) -> (Arc<Pager>, BLinkTree, std::path::PathBuf) {
    let path = test_dir().join(name);
    let _ = std::fs::remove_file(&path);
    let pager = Arc::new(Pager::open(&path).expect("open pager"));
    let tree = BLinkTree::new(pager.clone()).expect("new tree");
    (pager, tree, path)
}

#[test]
fn blink_split_grows_height() {
    let (_pager, tree, _path) = open_tree("split.db");
    // Insert enough keys to force a split; interior descent must still find all.
    let n = 200usize;
    for i in 0..n {
        let k = format!("key_{:05}", i);
        tree.insert(k.clone().into_bytes(), format!("val_{}", i).into_bytes(), 1)
            .expect("insert");
    }
    for i in 0..n {
        let k = format!("key_{:05}", i);
        let got = tree.search(k.as_bytes(), 1).expect("search");
        assert_eq!(got, Some(format!("val_{}", i).into_bytes()), "key {} lost after splits", k);
    }
}

#[test]
fn blink_merge_preserves_order() {
    let (_pager, tree, _path) = open_tree("merge.db");
    for i in 0..50usize {
        let k = format!("k{:03}", i);
        tree.insert(k.into_bytes(), b"v".to_vec(), 1).expect("insert");
    }
    // Delete the upper half; coalesce must keep remaining keys in order.
    for i in 25..50usize {
        let k = format!("k{:03}", i);
        assert!(tree.delete(k.as_bytes(), 2).expect("delete"), "delete {}", k);
    }
    let rows = tree.scan_all(2).expect("scan");
    let mut keys: Vec<Vec<u8>> = rows.into_iter().map(|(k, _)| k).collect();
    keys.sort();
    let expected: Vec<Vec<u8>> = (0..25usize).map(|i| format!("k{:03}", i).into_bytes()).collect();
    assert_eq!(keys, expected, "ordered scan after merge mismatch");
}

#[test]
fn scan_newest_first_matches_search() {
    let (_pager, tree, _path) = open_tree("scanvis.db");
    // Duplicate-key versions: newest visible at ts must win in both search and scan.
    tree.insert(b"dup".to_vec(), b"v1".to_vec(), 1).expect("insert v1");
    tree.insert(b"dup".to_vec(), b"v2".to_vec(), 2).expect("insert v2");
    tree.insert(b"dup".to_vec(), b"v3".to_vec(), 3).expect("insert v3");
    let via_search = tree.search(b"dup", 3).expect("search");
    assert_eq!(via_search, Some(b"v3".to_vec()));
    let rows = tree.scan_all(3).expect("scan");
    let found = rows.iter().find(|(k, _)| k.as_slice() == b"dup").expect("dup in scan");
    assert_eq!(found.1, b"v3".to_vec(), "scan disagrees with search on newest version");
    // Older snapshot must see v1.
    let old_search = tree.search(b"dup", 1).expect("old search");
    assert_eq!(old_search, Some(b"v1".to_vec()));
    let old_rows = tree.scan_all(1).expect("old scan");
    let old_found = old_rows.iter().find(|(k, _)| k.as_slice() == b"dup").expect("dup in old scan");
    assert_eq!(old_found.1, b"v1".to_vec());
}
