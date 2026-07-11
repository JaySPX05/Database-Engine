//! The lowest layer of our storage engine: reading and writing fixed-size
//! pages to a single database file. Everything above this (heap files,
//! B-tree nodes) will work in terms of page numbers, never raw file
//! offsets — the Pager is the only thing that knows how page numbers map
//! to bytes on disk.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// 4KB matches a common OS/filesystem block size, so one page read/write
/// tends to correspond to one physical disk I/O. This is the same choice
/// SQLite makes by default.
pub const PAGE_SIZE: usize = 4096;

/// A single page: a fixed-size buffer of raw bytes. `[u8; PAGE_SIZE]` is
/// a Rust array — its size is fixed at compile time, unlike `Vec<u8>`
/// which can grow/shrink. We use an array here because pages are never
/// resized; a Page is always exactly PAGE_SIZE bytes, full stop.
#[derive(Clone)]
pub struct Page {
    data: [u8; PAGE_SIZE],
}

impl Page {
    pub fn new() -> Self {
        Page { data: [0u8; PAGE_SIZE] }
    }

    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.data
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.data
    }
}

impl Default for Page {
    fn default() -> Self {
        Page::new()
    }
}

/// Manages reading and writing fixed-size pages to/from a single
/// on-disk file.
pub struct Pager {
    file: File,
    page_count: u64,
}

impl Pager {
    /// Open the database file at `path`, creating it if it doesn't exist.
    /// If it already exists, `page_count` is derived from its length —
    /// this is how we "recover" our page bookkeeping after a restart.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        let len = file.metadata()?.len();
        // Integer division: any partial trailing page (which shouldn't
        // happen in a healthy file, but could after a crash) is simply
        // not counted as a usable page.
        let page_count = len / PAGE_SIZE as u64;

        Ok(Pager { file, page_count })
    }

    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Read the page at `page_no` from disk into memory.
    pub fn read_page(&mut self, page_no: u64) -> io::Result<Page> {
        let mut page = Page::new();
        let offset = page_no * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        // read_exact fails with an error (rather than silently returning
        // fewer bytes) if it can't fill the whole buffer — exactly what
        // we want, since a short read means something is corrupt.
        self.file.read_exact(page.as_bytes_mut())?;
        Ok(page)
    }

    /// Write `page`'s contents to `page_no`'s slot in the file.
    ///
    /// Note: this does NOT by itself guarantee the data survives a power
    /// loss. The OS may buffer this write in memory and not put it on
    /// physical disk immediately. For that guarantee, see `flush()` — and
    /// we'll revisit this properly once we build the write-ahead log,
    /// which is the *correct* place to reason about crash durability.
    pub fn write_page(&mut self, page_no: u64, page: &Page) -> io::Result<()> {
        let offset = page_no * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(page.as_bytes())?;
        Ok(())
    }

    /// Allocate a brand-new, zero-filled page at the end of the file and
    /// return its page number. This is how the heap file (next phase)
    /// will get fresh space to store documents.
    pub fn allocate_page(&mut self) -> io::Result<u64> {
        let page_no = self.page_count;
        self.write_page(page_no, &Page::new())?;
        self.page_count += 1;
        Ok(page_no)
    }

    /// Force the OS to flush any buffered writes to physical disk
    /// (fsync). Slow, but necessary at specific moments if you actually
    /// care about durability across crashes.
    pub fn flush(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Tests need a real file on disk. We put each test's file in the OS
    /// temp dir with a name unique to the test + process, and clean up
    /// afterward so repeated test runs don't collide or leak files.
    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("docdb_test_{name}_{}.db", std::process::id()));
        path
    }

    #[test]
    fn allocate_and_read_write_page() {
        let path = temp_path("alloc_rw");
        let _ = fs::remove_file(&path); // ignore error if it doesn't exist yet

        let mut pager = Pager::open(&path).unwrap();
        assert_eq!(pager.page_count(), 0);

        let page_no = pager.allocate_page().unwrap();
        assert_eq!(page_no, 0);
        assert_eq!(pager.page_count(), 1);

        let mut page = Page::new();
        page.as_bytes_mut()[0..5].copy_from_slice(b"hello");
        pager.write_page(page_no, &page).unwrap();

        let read_back = pager.read_page(page_no).unwrap();
        assert_eq!(&read_back.as_bytes()[0..5], b"hello");

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reopening_the_file_preserves_page_count() {
        let path = temp_path("reopen");
        let _ = fs::remove_file(&path);

        {
            let mut pager = Pager::open(&path).unwrap();
            pager.allocate_page().unwrap();
            pager.allocate_page().unwrap();
            pager.flush().unwrap();
        } // `pager` (and its File handle) is dropped here, closing the file

        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.page_count(), 2);

        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn multiple_pages_dont_overlap() {
        let path = temp_path("multi");
        let _ = fs::remove_file(&path);

        let mut pager = Pager::open(&path).unwrap();
        let p0 = pager.allocate_page().unwrap();
        let p1 = pager.allocate_page().unwrap();

        let mut page0 = Page::new();
        page0.as_bytes_mut()[0] = 0xAA;
        let mut page1 = Page::new();
        page1.as_bytes_mut()[0] = 0xBB;

        pager.write_page(p0, &page0).unwrap();
        pager.write_page(p1, &page1).unwrap();

        // Read them back in reverse order just to prove there's no
        // aliasing / overlap between page slots.
        assert_eq!(pager.read_page(p1).unwrap().as_bytes()[0], 0xBB);
        assert_eq!(pager.read_page(p0).unwrap().as_bytes()[0], 0xAA);

        fs::remove_file(&path).unwrap();
    }
}
