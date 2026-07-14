//! The Collection API: composes a `HeapFile` (where document bytes live)
//! and a `BTree` (indexing `_id` -> RecordId) into the interface
//! application code actually talks to. This is where the previous four
//! phases stop being separate pieces and start being a database.

use crate::btree::BTree;
use crate::document::{Document, Value};
use crate::heap::HeapFile;
use crate::query;
use std::io;
use std::path::{Path, PathBuf};

pub struct Collection {
    heap: HeapFile,
    index: BTree,
}

impl Collection {
    /// Open (or create) a collection. `path_prefix` names a pair of
    /// files: `{path_prefix}.heap.db` for document storage and
    /// `{path_prefix}.index.db` for the `_id` index — e.g. passing
    /// "data/people" creates "data/people.heap.db" and "data/people.index.db".
    pub fn open(path_prefix: impl AsRef<Path>) -> io::Result<Self> {
        let prefix = path_prefix.as_ref();
        let heap = HeapFile::open(with_suffix(prefix, ".heap.db"))?;
        let index = BTree::open(with_suffix(prefix, ".index.db"))?;
        Ok(Collection { heap, index })
    }

    /// Insert a document. If it doesn't already carry a string `_id`
    /// field, one is generated automatically. Returns the `_id` used —
    /// callers need this to look the document up again later.
    pub fn insert(&mut self, mut doc: Document) -> io::Result<String> {
        let id = match doc.get("_id") {
            Some(Value::Str(existing)) => existing.clone(),
            _ => {
                let counter = self.heap.next_counter_value()?;
                let generated = format!("{counter:016x}");
                doc.insert("_id", generated.clone());
                generated
            }
        };

        let record_id = self.heap.insert(&doc)?;
        self.index.insert(&id, record_id)?;
        Ok(id)
    }

    /// Fetch a document by its `_id`. This is the two-step "index then
    /// storage" lookup: find the RecordId via the B-Tree (O(log n)),
    /// then fetch the actual bytes from the heap file.
    pub fn find_by_id(&mut self, id: &str) -> io::Result<Option<Document>> {
        match self.index.get(id)? {
            Some(record_id) => Ok(Some(self.heap.get(record_id)?)),
            None => Ok(None),
        }
    }

    /// Replace the document stored under `id` with `new_doc`, preserving
    /// the `_id`. Returns `false` (and does nothing) if `id` doesn't exist.
    ///
    /// Note this is implemented as delete-then-reinsert rather than an
    /// in-place edit — the new document may be a completely different
    /// size, so it may land on entirely different heap pages. The index
    /// is what makes that invisible to callers: `id` still resolves
    /// correctly no matter where the bytes physically moved to.
    pub fn update_by_id(&mut self, id: &str, mut new_doc: Document) -> io::Result<bool> {
        let Some(old_record_id) = self.index.get(id)? else {
            return Ok(false);
        };

        new_doc.insert("_id", id.to_string());
        self.heap.delete(old_record_id)?;
        let new_record_id = self.heap.insert(&new_doc)?;
        self.index.insert(id, new_record_id)?; // upsert overwrites the old mapping

        Ok(true)
    }

    /// Delete a document by `_id`. Returns `false` if it didn't exist.
    pub fn delete_by_id(&mut self, id: &str) -> io::Result<bool> {
        let Some(record_id) = self.index.get(id)? else {
            return Ok(false);
        };
        self.heap.delete(record_id)?;
        self.index.delete(id)?;
        Ok(true)
    }

    /// Every document in the collection, in `_id` order (a side effect
    /// of the index being a sorted structure — this is essentially free
    /// given what we already built).
    pub fn all(&mut self) -> io::Result<Vec<Document>> {
        let mut docs = Vec::new();
        for (_id, record_id) in self.index.scan_all()? {
            docs.push(self.heap.get(record_id)?);
        }
        Ok(docs)
    }

    /// Every document matching `query` (MongoDB-style filter document —
    /// see the `query` module). This scans every document via `all()`
    /// and filters in memory; only `_id` lookups are index-accelerated
    /// right now.
    pub fn find(&mut self, filter: &Document) -> io::Result<Vec<Document>> {
        Ok(self.all()?.into_iter().filter(|doc| query::matches(doc, filter)).collect())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.heap.flush()?;
        self.index.flush()
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os_string = path.as_os_str().to_owned();
    os_string.push(suffix);
    PathBuf::from(os_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_prefix(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("docdb_collection_test_{name}_{}", std::process::id()));
        path
    }

    fn cleanup(prefix: &Path) {
        let _ = fs::remove_file(with_suffix(prefix, ".heap.db"));
        let _ = fs::remove_file(with_suffix(prefix, ".index.db"));
    }

    #[test]
    fn insert_without_id_auto_generates_one() {
        let prefix = temp_prefix("autoid");
        cleanup(&prefix);

        let mut coll = Collection::open(&prefix).unwrap();
        let mut doc = Document::new();
        doc.insert("name", "Alice");

        let id = coll.insert(doc).unwrap();
        assert!(!id.is_empty());

        let fetched = coll.find_by_id(&id).unwrap().unwrap();
        assert_eq!(fetched.get("_id"), Some(&Value::Str(id)));
        assert_eq!(fetched.get("name"), Some(&Value::Str("Alice".to_string())));

        cleanup(&prefix);
    }

    #[test]
    fn insert_with_explicit_id_is_preserved() {
        let prefix = temp_prefix("explicitid");
        cleanup(&prefix);

        let mut coll = Collection::open(&prefix).unwrap();
        let mut doc = Document::new();
        doc.insert("_id", "custom-id");
        doc.insert("name", "Bob");

        let id = coll.insert(doc).unwrap();
        assert_eq!(id, "custom-id");
        assert!(coll.find_by_id("custom-id").unwrap().is_some());

        cleanup(&prefix);
    }

    #[test]
    fn update_replaces_content_but_keeps_id() {
        let prefix = temp_prefix("update");
        cleanup(&prefix);

        let mut coll = Collection::open(&prefix).unwrap();
        let mut doc = Document::new();
        doc.insert("name", "Alice");
        doc.insert("age", 30);
        let id = coll.insert(doc).unwrap();

        let mut updated = Document::new();
        updated.insert("name", "Alice");
        updated.insert("age", 31);
        assert!(coll.update_by_id(&id, updated).unwrap());

        let fetched = coll.find_by_id(&id).unwrap().unwrap();
        assert_eq!(fetched.get("age"), Some(&Value::Int(31)));
        assert_eq!(fetched.get("_id"), Some(&Value::Str(id)));

        cleanup(&prefix);
    }

    #[test]
    fn update_on_missing_id_returns_false() {
        let prefix = temp_prefix("update_missing");
        cleanup(&prefix);

        let mut coll = Collection::open(&prefix).unwrap();
        let doc = Document::new();
        assert!(!coll.update_by_id("nonexistent", doc).unwrap());

        cleanup(&prefix);
    }

    #[test]
    fn delete_removes_document() {
        let prefix = temp_prefix("delete");
        cleanup(&prefix);

        let mut coll = Collection::open(&prefix).unwrap();
        let mut doc = Document::new();
        doc.insert("name", "Temp");
        let id = coll.insert(doc).unwrap();

        assert!(coll.delete_by_id(&id).unwrap());
        assert!(coll.find_by_id(&id).unwrap().is_none());
        assert!(!coll.delete_by_id(&id).unwrap()); // second delete: nothing to do

        cleanup(&prefix);
    }

    #[test]
    fn all_returns_every_document() {
        let prefix = temp_prefix("all");
        cleanup(&prefix);

        let mut coll = Collection::open(&prefix).unwrap();
        for name in ["Alice", "Bob", "Carol"] {
            let mut doc = Document::new();
            doc.insert("name", name);
            coll.insert(doc).unwrap();
        }

        let all_docs = coll.all().unwrap();
        assert_eq!(all_docs.len(), 3);

        cleanup(&prefix);
    }

    #[test]
    fn data_survives_reopening() {
        let prefix = temp_prefix("persist");
        cleanup(&prefix);

        let id = {
            let mut coll = Collection::open(&prefix).unwrap();
            let mut doc = Document::new();
            doc.insert("durable", true);
            let id = coll.insert(doc).unwrap();
            coll.flush().unwrap();
            id
        };

        let mut coll = Collection::open(&prefix).unwrap();
        assert!(coll.find_by_id(&id).unwrap().is_some());

        cleanup(&prefix);
    }
}
