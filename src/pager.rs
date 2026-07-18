//! The lowest layer of our storage engine: reading and writing fixed-size
//! pages to a single database file. Everything above this (heap files,
//! B-tree nodes) will work in terms of page numbers, never raw file
//! offsets — the Pager is the only thing that knows how page numbers map
//! to bytes on disk.
//!
//! Since Phase 8, the Pager is also where crash durability lives: every
//! write goes through a write-ahead log first (see `wal.rs`), either as
//! its own implicit one-page transaction ("auto-commit") or as part of an
//! explicit multi-page transaction the caller controls.

use crate::wal::Wal;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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
/// on-disk file, with crash-safe writes via a write-ahead log.
pub struct Pager {
    file: File,
    wal: Wal,
    page_count: u64,
    /// Pages written during the current transaction, buffered in memory
    /// so we can (a) read them back before they're committed
    /// ("read-your-own-writes") and (b) apply them all to the main file
    /// at once on commit.
    pending: Vec<(u64, Page)>,
    in_txn: bool,
    /// The WAL's length when the current transaction began, so a
    /// rollback knows exactly how much of the log to discard.
    txn_wal_start_len: u64,
}

impl Pager {
    /// Open the database file at `path`, creating it if it doesn't exist.
    /// Also opens (or creates) its companion `<path>-wal` log file, and
    /// performs crash recovery: if the log contains any committed writes
    /// that never made it into the main file (i.e. the process crashed
    /// between "log fsync'd" and "main file updated"), they're replayed
    /// now, before anything else touches this database.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let mut file = OpenOptions::new().read(true).write(true).create(true).open(path)?;

        let mut wal = Wal::open(wal_path_for(path))?;
        let committed = wal.read_committed_frames()?;
        if !committed.is_empty() {
            for (page_no, page) in &committed {
                apply_raw(&mut file, *page_no, page)?;
            }
            file.sync_all()?;
            wal.truncate_to(0)?;
        }

        // Integer division: any partial trailing page (which shouldn't
        // happen in a healthy file, but could after a crash) is simply
        // not counted as a usable page.
        let len = file.metadata()?.len();
        let page_count = len / PAGE_SIZE as u64;

        Ok(Pager {
            file,
            wal,
            page_count,
            pending: Vec::new(),
            in_txn: false,
            txn_wal_start_len: 0,
        })
    }

    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Begin an explicit transaction: subsequent `write_page` calls are
    /// buffered (logged, but not yet applied to the main file) until
    /// `commit_transaction` or `rollback_transaction` is called. Use this
    /// when a single logical operation touches multiple pages that must
    /// all succeed or all fail together.
    pub fn begin_transaction(&mut self) -> io::Result<()> {
        debug_assert!(!self.in_txn, "transactions do not nest");
        self.txn_wal_start_len = self.wal.len()?;
        self.pending.clear();
        self.in_txn = true;
        Ok(())
    }

    /// Make every page written since `begin_transaction` durable and
    /// visible. The commit marker + fsync is the actual point of no
    /// return: once that completes, all of this transaction's writes are
    /// guaranteed to survive a crash, even before we've touched the main
    /// file at all.
    pub fn commit_transaction(&mut self) -> io::Result<()> {
        debug_assert!(self.in_txn, "commit called with no active transaction");

        self.wal.append_commit_frame()?;
        self.wal.fsync()?; // <-- durability point

        for (page_no, page) in &self.pending {
            apply_raw(&mut self.file, *page_no, page)?;
        }
        self.file.sync_all()?;

        self.wal.truncate_to(0)?;
        self.pending.clear();
        self.in_txn = false;
        Ok(())
    }

    /// Abandon the current transaction: none of its writes take effect,
    /// and the log is trimmed back to before it began.
    pub fn rollback_transaction(&mut self) -> io::Result<()> {
        debug_assert!(self.in_txn, "rollback called with no active transaction");
        self.wal.truncate_to(self.txn_wal_start_len)?;
        self.pending.clear();
        self.in_txn = false;
        Ok(())
    }

    /// Read the page at `page_no`. If it's been written during the
    /// current (uncommitted) transaction, that version is returned
    /// instead of what's on disk — otherwise a transaction couldn't see
    /// its own writes before committing.
    pub fn read_page(&mut self, page_no: u64) -> io::Result<Page> {
        if let Some((_, page)) = self.pending.iter().rev().find(|(pn, _)| *pn == page_no) {
            return Ok(page.clone());
        }
        read_raw(&mut self.file, page_no)
    }

    /// Write `page`'s contents to `page_no`'s slot.
    ///
    /// If called inside an explicit transaction, this only logs and
    /// buffers the write — it takes effect at `commit_transaction`. If
    /// called standalone (no explicit transaction active), it's treated
    /// as its own one-page transaction: logged, fsync'd, applied, and
    /// checkpointed immediately. Either way, every write that returns
    /// `Ok` is crash-safe — that guarantee is no longer opt-in.
    pub fn write_page(&mut self, page_no: u64, page: &Page) -> io::Result<()> {
        if self.in_txn {
            self.wal.append_page_frame(page_no, page)?;
            self.pending.retain(|(pn, _)| *pn != page_no);
            self.pending.push((page_no, page.clone()));
            Ok(())
        } else {
            self.wal.append_page_frame(page_no, page)?;
            self.wal.append_commit_frame()?;
            self.wal.fsync()?; // durability point for this single-page auto-commit
            apply_raw(&mut self.file, page_no, page)?;
            self.file.sync_all()?;
            self.wal.truncate_to(0)?;
            Ok(())
        }
    }

    /// Allocate a brand-new, zero-filled page at the end of the file and
    /// return its page number.
    pub fn allocate_page(&mut self) -> io::Result<u64> {
        let page_no = self.page_count;
        self.write_page(page_no, &Page::new())?;
        self.page_count += 1;
        Ok(page_no)
    }

    /// Force the OS to flush any buffered writes to physical disk
    /// (fsync). Every write is already durable the moment it returns
    /// `Ok` (see `write_page`), so this is mostly useful as an explicit
    /// "make sure everything's settled" call at natural checkpoints.
    pub fn flush(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}

fn wal_path_for(path: &Path) -> PathBuf {
    let mut os_string = path.as_os_str().to_owned();
    os_string.push("-wal");
    PathBuf::from(os_string)
}

fn apply_raw(file: &mut File, page_no: u64, page: &Page) -> io::Result<()> {
    let offset = page_no * PAGE_SIZE as u64;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(page.as_bytes())
}

fn read_raw(file: &mut File, page_no: u64) -> io::Result<Page> {
    let mut page = Page::new();
    let offset = page_no * PAGE_SIZE as u64;
    file.seek(SeekFrom::Start(offset))?;
    // read_exact fails with an error (rather than silently returning
    // fewer bytes) if it can't fill the whole buffer — exactly what we
    // want, since a short read means something is corrupt or missing.
    file.read_exact(page.as_bytes_mut())?;
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Tests need a real file on disk. We put each test's file in the OS
    /// temp dir with a name unique to the test + process, and clean up
    /// afterward so repeated test runs don't collide or leak files.
    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("docdb_test_{name}_{}.db", std::process::id()));
        path
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(wal_path_for(path));
    }

    #[test]
    fn allocate_and_read_write_page() {
        let path = temp_path("alloc_rw");
        cleanup(&path);

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

        cleanup(&path);
    }

    #[test]
    fn reopening_the_file_preserves_page_count() {
        let path = temp_path("reopen");
        cleanup(&path);

        {
            let mut pager = Pager::open(&path).unwrap();
            pager.allocate_page().unwrap();
            pager.allocate_page().unwrap();
            pager.flush().unwrap();
        } // `pager` (and its File handle) is dropped here, closing the file

        let pager = Pager::open(&path).unwrap();
        assert_eq!(pager.page_count(), 2);

        cleanup(&path);
    }

    #[test]
    fn multiple_pages_dont_overlap() {
        let path = temp_path("multi");
        cleanup(&path);

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

        cleanup(&path);
    }

    #[test]
    fn explicit_transaction_commit_persists_multiple_pages() {
        let path = temp_path("txn_commit");
        cleanup(&path);

        let mut pager = Pager::open(&path).unwrap();
        pager.begin_transaction().unwrap();

        let mut p0 = Page::new();
        p0.as_bytes_mut()[0] = 1;
        let mut p1 = Page::new();
        p1.as_bytes_mut()[0] = 2;
        pager.write_page(0, &p0).unwrap();
        pager.write_page(1, &p1).unwrap();

        pager.commit_transaction().unwrap();

        assert_eq!(pager.read_page(0).unwrap().as_bytes()[0], 1);
        assert_eq!(pager.read_page(1).unwrap().as_bytes()[0], 2);

        cleanup(&path);
    }

    #[test]
    fn explicit_transaction_rollback_discards_writes() {
        let path = temp_path("txn_rollback");
        cleanup(&path);

        let mut pager = Pager::open(&path).unwrap();

        let mut original = Page::new();
        original.as_bytes_mut()[0] = 9;
        pager.write_page(0, &original).unwrap(); // auto-commit: page 0 = 9

        pager.begin_transaction().unwrap();
        let mut changed = Page::new();
        changed.as_bytes_mut()[0] = 42;
        pager.write_page(0, &changed).unwrap();

        // Within the transaction, we should see our own uncommitted write.
        assert_eq!(pager.read_page(0).unwrap().as_bytes()[0], 42);

        pager.rollback_transaction().unwrap();

        // After rollback, the original committed value is what's there.
        assert_eq!(pager.read_page(0).unwrap().as_bytes()[0], 9);

        cleanup(&path);
    }

    #[test]
    fn crash_recovery_replays_committed_but_unapplied_writes() {
        let path = temp_path("crash_recovery");
        cleanup(&path);

        // Simulate a crash that happened AFTER the WAL was durably
        // fsync'd but BEFORE the corresponding write reached the main
        // file — by writing directly to the WAL and never touching the
        // main file at all.
        {
            let mut wal = Wal::open(wal_path_for(&path)).unwrap();
            let mut page = Page::new();
            page.as_bytes_mut()[0..5].copy_from_slice(b"crash");
            wal.append_page_frame(3, &page).unwrap();
            wal.append_commit_frame().unwrap();
            wal.fsync().unwrap();
        }

        // Opening the Pager must replay that committed write.
        let mut pager = Pager::open(&path).unwrap();
        let recovered = pager.read_page(3).unwrap();
        assert_eq!(&recovered.as_bytes()[0..5], b"crash");

        cleanup(&path);
    }

    #[test]
    fn crash_recovery_ignores_uncommitted_frames() {
        let path = temp_path("uncommitted");
        cleanup(&path);

        // Simulate a crash mid-transaction: a page frame was logged, but
        // no commit frame ever followed it.
        {
            let mut wal = Wal::open(wal_path_for(&path)).unwrap();
            let mut page = Page::new();
            page.as_bytes_mut()[0] = 0xFF;
            wal.append_page_frame(5, &page).unwrap();
            wal.fsync().unwrap();
            // No commit frame — this transaction never completed.
        }

        let mut pager = Pager::open(&path).unwrap();
        // The uncommitted write must NOT have been applied.
        assert_eq!(pager.page_count(), 0);
        assert!(pager.read_page(5).is_err());

        cleanup(&path);
    }
}

