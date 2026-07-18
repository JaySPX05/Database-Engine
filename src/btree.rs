//! A disk-backed B+Tree index mapping String keys to RecordIds.
//!
//! Structure:
//!   - Leaf nodes hold sorted (key, RecordId) pairs and a pointer to the
//!     next leaf, forming a linked chain across all leaves left-to-right.
//!   - Internal nodes hold N separator keys and N+1 child page pointers.
//!     To find a search key, find the first separator greater than it and
//!     descend into the child just before it.
//!   - Insertion may overflow a page, triggering a split; a split at the
//!     root grows the tree by one level.
//!
//! Simplification: deletion removes the key from its leaf but does not
//! merge/rebalance underflowed nodes. Real B-Trees do this; we skip it
//! here to keep the implementation approachable — it costs some wasted
//! space after heavy deletion, never correctness.

use crate::heap::RecordId;
use crate::pager::{Page, Pager, PAGE_SIZE};
use std::io;
use std::path::Path;

const NONE: u64 = u64::MAX;
const META_PAGE: u64 = 0;

const LEAF_TAG: u8 = 1;
const INTERNAL_TAG: u8 = 2;
/// tag(1) + count(2) + next_leaf(8). Internal nodes waste the last 8
/// bytes (always NONE) to keep the header a uniform size for both node
/// kinds — a small tradeoff for simpler code.
const HEADER_SIZE: usize = 11;
const MAX_BODY: usize = PAGE_SIZE - HEADER_SIZE;

/// An in-memory, owned representation of one page's contents. We always
/// fully decode a page into this before touching it, and fully encode it
/// back before writing — the on-disk byte layout never leaks past this
/// boundary.
enum Node {
    Leaf { next_leaf: u64, entries: Vec<(String, u64)> },
    Internal { children: Vec<u64>, keys: Vec<String> },
}

impl Node {
    fn body_size(&self) -> usize {
        match self {
            Node::Leaf { entries, .. } => leaf_body_size(entries),
            Node::Internal { keys, .. } => internal_body_size(keys),
        }
    }

    fn encode(&self, page: &mut Page) {
        let buf = page.as_bytes_mut();
        match self {
            Node::Leaf { next_leaf, entries } => {
                buf[0] = LEAF_TAG;
                write_u16(buf, 1, entries.len() as u16);
                write_u64(buf, 3, *next_leaf);
                let mut offset = HEADER_SIZE;
                for (key, rid) in entries {
                    offset = write_key(buf, offset, key);
                    write_u64(buf, offset, *rid);
                    offset += 8;
                }
            }
            Node::Internal { children, keys } => {
                buf[0] = INTERNAL_TAG;
                write_u16(buf, 1, keys.len() as u16);
                write_u64(buf, 3, NONE);
                let mut offset = HEADER_SIZE;
                offset = { write_u64(buf, offset, children[0]); offset + 8 };
                for i in 0..keys.len() {
                    offset = write_key(buf, offset, &keys[i]);
                    write_u64(buf, offset, children[i + 1]);
                    offset += 8;
                }
            }
        }
    }

    fn decode(page: &Page) -> Node {
        let buf = page.as_bytes();
        let tag = buf[0];
        let count = read_u16(buf, 1) as usize;
        let next_leaf = read_u64(buf, 3);
        let mut offset = HEADER_SIZE;

        match tag {
            LEAF_TAG => {
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let (key, new_offset) = read_key(buf, offset);
                    let rid = read_u64(buf, new_offset);
                    offset = new_offset + 8;
                    entries.push((key, rid));
                }
                Node::Leaf { next_leaf, entries }
            }
            INTERNAL_TAG => {
                let mut children = Vec::with_capacity(count + 1);
                children.push(read_u64(buf, offset));
                offset += 8;
                let mut keys = Vec::with_capacity(count);
                for _ in 0..count {
                    let (key, new_offset) = read_key(buf, offset);
                    let child = read_u64(buf, new_offset);
                    offset = new_offset + 8;
                    keys.push(key);
                    children.push(child);
                }
                Node::Internal { children, keys }
            }
            other => panic!("corrupt btree page: unknown tag {other}"),
        }
    }
}

fn leaf_body_size(entries: &[(String, u64)]) -> usize {
    entries.iter().map(|(k, _)| 2 + k.len() + 8).sum()
}
fn internal_body_size(keys: &[String]) -> usize {
    8 + keys.iter().map(|k| 2 + k.len() + 8).sum::<usize>()
}

fn write_u16(buf: &mut [u8], offset: usize, value: u16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn read_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(buf[offset..offset + 2].try_into().unwrap())
}
fn write_u64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn read_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
}
fn write_key(buf: &mut [u8], offset: usize, s: &str) -> usize {
    let bytes = s.as_bytes();
    write_u16(buf, offset, bytes.len() as u16);
    let start = offset + 2;
    buf[start..start + bytes.len()].copy_from_slice(bytes);
    start + bytes.len()
}
fn read_key(buf: &[u8], offset: usize) -> (String, usize) {
    let len = read_u16(buf, offset) as usize;
    let start = offset + 2;
    let key = String::from_utf8(buf[start..start + len].to_vec()).unwrap();
    (key, start + len)
}

/// A B+Tree index over `String` keys, backed by its own file.
pub struct BTree {
    pager: Pager,
    root: u64,
}

impl BTree {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut pager = Pager::open(path)?;

        if pager.page_count() == 0 {
            let meta_no = pager.allocate_page()?;
            debug_assert_eq!(meta_no, META_PAGE);

            let root_no = pager.allocate_page()?;
            let mut meta = Page::new();
            write_u64(meta.as_bytes_mut(), 0, root_no);
            pager.write_page(META_PAGE, &meta)?;

            let empty_root = Node::Leaf { next_leaf: NONE, entries: Vec::new() };
            let mut root_page = Page::new();
            empty_root.encode(&mut root_page);
            pager.write_page(root_no, &root_page)?;
        }

        let meta = pager.read_page(META_PAGE)?;
        let root = read_u64(meta.as_bytes(), 0);
        Ok(BTree { pager, root })
    }

    /// Insert or update the value for `key` (this is an upsert: an
    /// existing key's RecordId is overwritten, matching how a unique
    /// primary-key index should behave).
    pub fn insert(&mut self, key: &str, rid: RecordId) -> io::Result<()> {
        // A single logical insert can cascade into several page writes:
        // a leaf split, then its parent splitting too, possibly all the
        // way up to a brand-new root. All of those pages need to become
        // durable together — a tree that's split at the leaf level but
        // not yet linked into its parent is a broken tree.
        self.pager.begin_transaction()?;

        let result: io::Result<()> = (|| {
            if let Some((promoted, new_right)) = self.insert_into(self.root, key, rid.0)? {
                // The root itself split — grow the tree by adding a new
                // root above the old one. This is the only way tree
                // height increases.
                let new_root = Node::Internal { children: vec![self.root, new_right], keys: vec![promoted] };
                let new_root_no = self.pager.allocate_page()?;
                let mut page = Page::new();
                new_root.encode(&mut page);
                self.pager.write_page(new_root_no, &page)?;

                self.root = new_root_no;
                let mut meta = Page::new();
                write_u64(meta.as_bytes_mut(), 0, self.root);
                self.pager.write_page(META_PAGE, &meta)?;
            }
            Ok(())
        })();

        match result {
            Ok(()) => self.pager.commit_transaction(),
            Err(e) => {
                let _ = self.pager.rollback_transaction();
                Err(e)
            }
        }
    }

    /// Recursively insert into the subtree rooted at `page_no`. Returns
    /// `Some((promoted_key, new_right_page))` if this node had to split,
    /// so the caller (the parent, or `insert` itself for the root) can
    /// link the new sibling in.
    fn insert_into(&mut self, page_no: u64, key: &str, rid_raw: u64) -> io::Result<Option<(String, u64)>> {
        let page = self.pager.read_page(page_no)?;
        let mut node = Node::decode(&page);

        // Phase 1: apply the insertion to this node (owned, in memory).
        match &mut node {
            Node::Leaf { entries, .. } => {
                match entries.binary_search_by(|(k, _)| k.as_str().cmp(key)) {
                    Ok(idx) => entries[idx].1 = rid_raw, // key exists: overwrite
                    Err(idx) => entries.insert(idx, (key.to_string(), rid_raw)),
                }
            }
            Node::Internal { children, keys } => {
                let idx = keys.iter().position(|k| key < k.as_str()).unwrap_or(keys.len());
                let child_no = children[idx];
                // Recursing here is safe even though `children`/`keys` are
                // borrowed from `node`: `node` is a plain local variable,
                // completely unrelated to `self`'s fields, so borrowing
                // one doesn't block mutably borrowing `self` for the
                // recursive call.
                if let Some((promoted, new_child)) = self.insert_into(child_no, key, rid_raw)? {
                    keys.insert(idx, promoted);
                    children.insert(idx + 1, new_child);
                }
            }
        }

        // Phase 2: if it still fits in a page, write it back and we're done.
        if node.body_size() <= MAX_BODY {
            let mut page = Page::new();
            node.encode(&mut page);
            self.pager.write_page(page_no, &page)?;
            return Ok(None);
        }

        // Phase 3: overflow — split this node into two.
        match node {
            Node::Leaf { next_leaf, mut entries } => {
                let mid = entries.len() / 2;
                let right_entries = entries.split_off(mid);
                let promoted_key = right_entries[0].0.clone();

                let right_page_no = self.pager.allocate_page()?;
                let right_node = Node::Leaf { next_leaf, entries: right_entries };
                let left_node = Node::Leaf { next_leaf: right_page_no, entries };

                let mut left_page = Page::new();
                left_node.encode(&mut left_page);
                self.pager.write_page(page_no, &left_page)?;

                let mut right_page = Page::new();
                right_node.encode(&mut right_page);
                self.pager.write_page(right_page_no, &right_page)?;

                Ok(Some((promoted_key, right_page_no)))
            }
            Node::Internal { mut children, mut keys } => {
                let mid = keys.len() / 2;
                let promoted_key = keys[mid].clone();

                let right_keys = keys.split_off(mid + 1);
                keys.pop(); // the promoted key itself doesn't stay in either child
                let right_children = children.split_off(mid + 1);

                let left_node = Node::Internal { children, keys };
                let right_node = Node::Internal { children: right_children, keys: right_keys };

                let right_page_no = self.pager.allocate_page()?;
                let mut left_page = Page::new();
                left_node.encode(&mut left_page);
                self.pager.write_page(page_no, &left_page)?;

                let mut right_page = Page::new();
                right_node.encode(&mut right_page);
                self.pager.write_page(right_page_no, &right_page)?;

                Ok(Some((promoted_key, right_page_no)))
            }
        }
    }

    /// Look up a single key. O(tree height), i.e. O(log n).
    pub fn get(&mut self, key: &str) -> io::Result<Option<RecordId>> {
        let mut page_no = self.root;
        loop {
            let page = self.pager.read_page(page_no)?;
            match Node::decode(&page) {
                Node::Leaf { entries, .. } => {
                    return Ok(entries
                        .binary_search_by(|(k, _)| k.as_str().cmp(key))
                        .ok()
                        .map(|idx| RecordId(entries[idx].1)));
                }
                Node::Internal { children, keys } => {
                    let idx = keys.iter().position(|k| key < k.as_str()).unwrap_or(keys.len());
                    page_no = children[idx];
                }
            }
        }
    }

    /// Remove a key. Does not merge/rebalance nodes (see module docs).
    pub fn delete(&mut self, key: &str) -> io::Result<Option<RecordId>> {
        let mut page_no = self.root;
        loop {
            let page = self.pager.read_page(page_no)?;
            match Node::decode(&page) {
                Node::Leaf { next_leaf, mut entries } => {
                    let removed = entries
                        .binary_search_by(|(k, _)| k.as_str().cmp(key))
                        .ok()
                        .map(|idx| RecordId(entries.remove(idx).1));

                    if removed.is_some() {
                        let node = Node::Leaf { next_leaf, entries };
                        let mut p = Page::new();
                        node.encode(&mut p);
                        self.pager.write_page(page_no, &p)?;
                    }
                    return Ok(removed);
                }
                Node::Internal { children, keys } => {
                    let idx = keys.iter().position(|k| key < k.as_str()).unwrap_or(keys.len());
                    page_no = children[idx];
                }
            }
        }
    }

    /// Return every (key, RecordId) pair in sorted order, by descending
    /// to the leftmost leaf and then walking the leaf chain sideways —
    /// this is the payoff of linking leaves together in a B+Tree.
    pub fn scan_all(&mut self) -> io::Result<Vec<(String, RecordId)>> {
        let mut page_no = self.root;
        loop {
            let page = self.pager.read_page(page_no)?;
            match Node::decode(&page) {
                Node::Leaf { .. } => break,
                Node::Internal { children, .. } => page_no = children[0],
            }
        }

        let mut results = Vec::new();
        loop {
            let page = self.pager.read_page(page_no)?;
            let Node::Leaf { next_leaf, entries } = Node::decode(&page) else {
                unreachable!("leaf chain should only ever point to leaves")
            };
            results.extend(entries.into_iter().map(|(k, rid)| (k, RecordId(rid))));

            if next_leaf == NONE {
                break;
            }
            page_no = next_leaf;
        }
        Ok(results)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.pager.flush()
    }

    #[cfg(test)]
    fn root_is_leaf(&mut self) -> io::Result<bool> {
        let page = self.pager.read_page(self.root)?;
        Ok(matches!(Node::decode(&page), Node::Leaf { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("docdb_btree_test_{name}_{}.db", std::process::id()));
        path
    }

    #[test]
    fn insert_and_get_single_key() {
        let path = temp_path("single");
        let _ = fs::remove_file(&path);

        let mut tree = BTree::open(&path).unwrap();
        tree.insert("alice", RecordId(42)).unwrap();

        assert_eq!(tree.get("alice").unwrap(), Some(RecordId(42)));
        assert_eq!(tree.get("bob").unwrap(), None);

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn upsert_overwrites_existing_key() {
        let path = temp_path("upsert");
        let _ = fs::remove_file(&path);

        let mut tree = BTree::open(&path).unwrap();
        tree.insert("alice", RecordId(1)).unwrap();
        tree.insert("alice", RecordId(2)).unwrap();

        assert_eq!(tree.get("alice").unwrap(), Some(RecordId(2)));

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn bulk_insert_forces_splits_and_stays_correct() {
        let path = temp_path("bulk");
        let _ = fs::remove_file(&path);

        let mut tree = BTree::open(&path).unwrap();

        // Insert out of order to exercise mid-tree insertion, not just
        // always-append-at-the-end.
        let mut keys: Vec<String> = (0..1000).map(|i| format!("key{i:05}")).collect();
        // Deterministic shuffle without pulling in a `rand` dependency:
        // a fixed-stride reordering is enough to avoid sorted-order bias.
        keys.sort_by_key(|k| {
            let n: usize = k[3..].parse().unwrap();
            (n * 37) % 1000
        });

        for (i, key) in keys.iter().enumerate() {
            tree.insert(key, RecordId(i as u64)).unwrap();
        }

        // The tree should have grown beyond a single leaf page by now.
        assert!(!tree.root_is_leaf().unwrap(), "expected root to have split into an internal node");

        // Every key should still be findable with the correct value.
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(tree.get(key).unwrap(), Some(RecordId(i as u64)), "lookup failed for {key}");
        }

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn scan_all_returns_sorted_order() {
        let path = temp_path("scan");
        let _ = fs::remove_file(&path);

        let mut tree = BTree::open(&path).unwrap();
        for key in ["banana", "apple", "cherry", "date"] {
            tree.insert(key, RecordId(key.len() as u64)).unwrap();
        }

        let all: Vec<String> = tree.scan_all().unwrap().into_iter().map(|(k, _)| k).collect();
        assert_eq!(all, vec!["apple", "banana", "cherry", "date"]);

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn delete_removes_key_without_disturbing_others() {
        let path = temp_path("delete");
        let _ = fs::remove_file(&path);

        let mut tree = BTree::open(&path).unwrap();
        tree.insert("a", RecordId(1)).unwrap();
        tree.insert("b", RecordId(2)).unwrap();
        tree.insert("c", RecordId(3)).unwrap();

        assert_eq!(tree.delete("b").unwrap(), Some(RecordId(2)));
        assert_eq!(tree.get("b").unwrap(), None);
        assert_eq!(tree.get("a").unwrap(), Some(RecordId(1)));
        assert_eq!(tree.get("c").unwrap(), Some(RecordId(3)));

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn data_survives_reopening_the_file() {
        let path = temp_path("persist");
        let _ = fs::remove_file(&path);

        {
            let mut tree = BTree::open(&path).unwrap();
            tree.insert("durable", RecordId(7)).unwrap();
            tree.flush().unwrap();
        }

        let mut tree = BTree::open(&path).unwrap();
        assert_eq!(tree.get("durable").unwrap(), Some(RecordId(7)));

        fs::remove_file(&path).unwrap();
    }
}
