//! The heap file: general-purpose storage for documents of any size,
//! built on top of the Pager. A document that doesn't fit in one page is
//! split across a chain of linked pages. Deleted documents' pages are
//! recycled via a free list rather than wasting space forever.

use crate::document::Document;
use crate::pager::{Page, Pager, PAGE_SIZE};
use std::io;
use std::path::Path;

/// Sentinel meaning "no next page" / "free list is empty". Page numbers
/// realistically never approach u64::MAX, so it's safe to reserve as an
/// out-of-band marker — we can't use 0 for this the way C uses a NULL
/// pointer, because 0 is a perfectly valid page number in our scheme.
const NONE: u64 = u64::MAX;

/// Every "in-use" chain page starts with: next_page (8 bytes) + payload_len (4 bytes).
const CHAIN_HEADER_SIZE: usize = 8 + 4;
const CHAIN_PAYLOAD_CAPACITY: usize = PAGE_SIZE - CHAIN_HEADER_SIZE;

/// Page 0 is reserved for heap-file-wide metadata — right now, just the
/// head of the free-page list.
const METADATA_PAGE: u64 = 0;

/// Identifies a stored document by the page where its byte chain begins.
/// This is the document's physical address. The indexing layer (next
/// phase) will map application-level keys ("_id": "abc123") to RecordIds.
///
/// This is a "tuple struct" — a lightweight wrapper around a single u64.
/// Its whole purpose is to stop us from ever accidentally passing a raw
/// page number where a RecordId was expected, or vice versa; the
/// compiler treats them as distinct types even though the data is
/// identical underneath.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordId(pub u64);

pub struct HeapFile {
    pager: Pager,
}

impl HeapFile {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut pager = Pager::open(path)?;

        if pager.page_count() == 0 {
            // Brand new file: allocate the metadata page and initialize
            // its free list to empty.
            let page_no = pager.allocate_page()?;
            debug_assert_eq!(page_no, METADATA_PAGE);
            let mut meta = Page::new();
            write_u64(meta.as_bytes_mut(), 0, NONE);
            pager.write_page(METADATA_PAGE, &meta)?;
        }

        Ok(HeapFile { pager })
    }

    /// Insert a document, returning the RecordId needed to fetch it later.
    pub fn insert(&mut self, doc: &Document) -> io::Result<RecordId> {
        let bytes = doc.to_bytes();

        // Split into page-sized pieces. `chunks()` is a slice method that
        // hands back an iterator of sub-slices, each up to the given
        // size — exactly what we need to know how many pages we'll use.
        let mut chunks: Vec<&[u8]> = bytes.chunks(CHAIN_PAYLOAD_CAPACITY).collect();
        if chunks.is_empty() {
            // An empty document still needs to occupy one page.
            chunks.push(&[]);
        }

        // A document may span several pages; all of them need to become
        // durable together, or not at all. Without a transaction, a
        // crash after writing page 1 of a 3-page chain would leave a
        // dangling, half-written document. `begin_transaction` buffers
        // every write below until we explicitly commit.
        self.pager.begin_transaction()?;

        // The closure-and-call-it-immediately trick: writing this as a
        // small closure lets us use `?` freely inside (early-return on
        // any error) while still guaranteeing we run commit-or-rollback
        // logic afterward, based on whether it succeeded.
        let result: io::Result<u64> = (|| {
            // We need every page's number before we can write the
            // *previous* page's "next" pointer, so allocate them all
            // up front.
            let mut page_numbers = Vec::with_capacity(chunks.len());
            for _ in 0..chunks.len() {
                page_numbers.push(self.allocate_page()?);
            }

            for (i, chunk) in chunks.iter().enumerate() {
                let next = page_numbers.get(i + 1).copied().unwrap_or(NONE);
                let mut page = Page::new();
                let buf = page.as_bytes_mut();
                write_u64(buf, 0, next);
                write_u32(buf, 8, chunk.len() as u32);
                buf[CHAIN_HEADER_SIZE..CHAIN_HEADER_SIZE + chunk.len()].copy_from_slice(chunk);
                self.pager.write_page(page_numbers[i], &page)?;
            }

            Ok(page_numbers[0])
        })();

        match result {
            Ok(head_page) => {
                self.pager.commit_transaction()?;
                Ok(RecordId(head_page))
            }
            Err(e) => {
                // Best-effort rollback: if this also fails (e.g. disk
                // already gone), we still propagate the original error,
                // which is the one the caller actually needs to see.
                let _ = self.pager.rollback_transaction();
                Err(e)
            }
        }
    }

    /// Read a document back by its RecordId, walking the page chain and
    /// reassembling the original bytes before decoding.
    pub fn get(&mut self, id: RecordId) -> io::Result<Document> {
        let mut bytes = Vec::new();
        let mut page_no = id.0;

        loop {
            let page = self.pager.read_page(page_no)?;
            let buf = page.as_bytes();
            let next = read_u64(buf, 0);
            let len = read_u32(buf, 8) as usize;
            bytes.extend_from_slice(&buf[CHAIN_HEADER_SIZE..CHAIN_HEADER_SIZE + len]);

            if next == NONE {
                break;
            }
            page_no = next;
        }

        // Convert our DecodeError into an io::Error so callers only ever
        // have to deal with one error type from this layer. `io::Error::new`
        // accepts anything implementing `std::error::Error`, which is
        // exactly why we bothered implementing that trait for DecodeError
        // back in Phase 2.
        Document::from_bytes(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Delete a document, returning every page in its chain to the free
    /// list so future inserts can reuse the space.
    pub fn delete(&mut self, id: RecordId) -> io::Result<()> {
        // Same reasoning as `insert`: freeing a multi-page chain touches
        // several pages (each one, plus the shared metadata page) that
        // need to change together.
        self.pager.begin_transaction()?;

        let result: io::Result<()> = (|| {
            let mut page_no = id.0;
            loop {
                let page = self.pager.read_page(page_no)?;
                let next = read_u64(page.as_bytes(), 0);
                self.free_page(page_no)?;
                if next == NONE {
                    break;
                }
                page_no = next;
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

    pub fn flush(&mut self) -> io::Result<()> {
        self.pager.flush()
    }

    /// Read-and-increment a counter stored in this heap's metadata page
    /// (at byte offset 8, right after the free-list head at offset 0).
    /// Collections use this to mint unique `_id` values without needing
    /// a separate file or an external UUID crate.
    pub fn next_counter_value(&mut self) -> io::Result<u64> {
        let meta = self.pager.read_page(METADATA_PAGE)?;
        let current = read_u64(meta.as_bytes(), 8);

        let mut updated = meta.clone();
        write_u64(updated.as_bytes_mut(), 8, current + 1);
        self.pager.write_page(METADATA_PAGE, &updated)?;

        Ok(current)
    }

    /// Get a page to write into: pop one off the free list if available,
    /// otherwise ask the Pager to grow the file with a fresh page.
    fn allocate_page(&mut self) -> io::Result<u64> {
        let meta = self.pager.read_page(METADATA_PAGE)?;
        let free_head = read_u64(meta.as_bytes(), 0);

        if free_head == NONE {
            return self.pager.allocate_page();
        }

        // Pop the head of the free list: the freed page's first 8 bytes
        // tell us what the *next* free page is (see `free_page` below).
        let freed_page = self.pager.read_page(free_head)?;
        let next_free = read_u64(freed_page.as_bytes(), 0);

        // IMPORTANT: clone the existing metadata page and edit only the
        // free-list-head bytes (0..8). The metadata page also holds the
        // id counter at bytes 8..16 (see `next_counter_value`) — writing
        // a brand-new `Page::new()` here would silently zero that
        // counter out every time a page got reused, which is exactly
        // the kind of bug that's invisible until IDs start colliding.
        let mut updated_meta = meta.clone();
        write_u64(updated_meta.as_bytes_mut(), 0, next_free);
        self.pager.write_page(METADATA_PAGE, &updated_meta)?;

        Ok(free_head)
    }

    /// Push a page onto the front of the free list. We repurpose its
    /// first 8 bytes to point at the previous free-list head — no extra
    /// storage needed for the free list itself.
    fn free_page(&mut self, page_no: u64) -> io::Result<()> {
        let meta = self.pager.read_page(METADATA_PAGE)?;
        let old_head = read_u64(meta.as_bytes(), 0);

        // The page being freed is fine to overwrite entirely — only its
        // first 8 bytes (the free-list "next" pointer) matter from now on.
        let mut freed = Page::new();
        write_u64(freed.as_bytes_mut(), 0, old_head);
        self.pager.write_page(page_no, &freed)?;

        // But the metadata page itself holds the id counter too — same
        // clone-and-edit fix as in allocate_page above.
        let mut updated_meta = meta.clone();
        write_u64(updated_meta.as_bytes_mut(), 0, page_no);
        self.pager.write_page(METADATA_PAGE, &updated_meta)?;

        Ok(())
    }
}

fn write_u64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn read_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
}
fn write_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Value;
    use std::fs;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("database_engine_heap_test_{name}_{}.db", std::process::id()));
        path
    }

    #[test]
    fn insert_and_get_small_document() {
        let path = temp_path("small");
        let _ = fs::remove_file(&path);

        let mut heap = HeapFile::open(&path).unwrap();
        let mut doc = Document::new();
        doc.insert("name", "Alice");
        doc.insert("age", 30);

        let id = heap.insert(&doc).unwrap();
        let fetched = heap.get(id).unwrap();
        assert_eq!(fetched, doc);

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn document_spanning_multiple_pages_round_trips() {
        let path = temp_path("large");
        let _ = fs::remove_file(&path);

        let mut heap = HeapFile::open(&path).unwrap();

        // Build a document big enough to force multiple chained pages:
        // ~1500 ints * 9 bytes each (1 tag byte + 8 value bytes) is well
        // over our ~4084-byte-per-page capacity.
        let big_array: Vec<Value> = (0..1500).map(Value::Int).collect();
        let mut doc = Document::new();
        doc.insert("numbers", big_array);

        let id = heap.insert(&doc).unwrap();

        // Confirm it actually spans more than one page before we even
        // check correctness — otherwise this test wouldn't be testing
        // what it claims to.
        let page0 = heap.pager.read_page(id.0).unwrap();
        let next = read_u64(page0.as_bytes(), 0);
        assert_ne!(next, NONE, "expected this document to span multiple pages");

        let fetched = heap.get(id).unwrap();
        assert_eq!(fetched, doc);

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn delete_frees_pages_for_reuse() {
        let path = temp_path("reuse");
        let _ = fs::remove_file(&path);

        let mut heap = HeapFile::open(&path).unwrap();

        let mut doc_a = Document::new();
        doc_a.insert("val", "a");
        let id_a = heap.insert(&doc_a).unwrap();

        heap.delete(id_a).unwrap();

        let mut doc_b = Document::new();
        doc_b.insert("val", "b");
        let id_b = heap.insert(&doc_b).unwrap();

        // The freed page should have been handed straight back out
        // rather than the file growing with a brand new page.
        assert_eq!(id_a.0, id_b.0, "expected the freed page to be reused");
        assert_eq!(heap.get(id_b).unwrap(), doc_b);

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn counter_survives_free_list_churn() {
        // Regression test: allocate_page/free_page used to overwrite the
        // ENTIRE metadata page with a fresh zeroed Page whenever the free
        // list was touched, silently wiping out the id counter stored at
        // byte offset 8. This churns the free list deliberately and
        // checks the counter keeps counting instead of resetting to 0.
        let path = temp_path("counter_survives");
        let _ = fs::remove_file(&path);

        let mut heap = HeapFile::open(&path).unwrap();
        assert_eq!(heap.next_counter_value().unwrap(), 0);
        assert_eq!(heap.next_counter_value().unwrap(), 1);

        let mut doc = Document::new();
        doc.insert("val", "x");
        let id = heap.insert(&doc).unwrap();
        heap.delete(id).unwrap(); // exercises free_page

        let id2 = heap.insert(&doc).unwrap(); // exercises allocate_page's reuse branch
        assert_eq!(id2.0, id.0, "expected the freed page to be reused");

        assert_eq!(heap.next_counter_value().unwrap(), 2, "counter must survive free-list churn");

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn data_survives_reopening_the_file() {
        let path = temp_path("persist");
        let _ = fs::remove_file(&path);

        let mut doc = Document::new();
        doc.insert("durable", true);

        let id = {
            let mut heap = HeapFile::open(&path).unwrap();
            let id = heap.insert(&doc).unwrap();
            heap.flush().unwrap();
            id
        }; // heap (and its file handle) dropped here

        let mut heap = HeapFile::open(&path).unwrap();
        assert_eq!(heap.get(id).unwrap(), doc);

        fs::remove_file(&path).unwrap();
    }
}
