//! The write-ahead log (WAL): the mechanism that makes writes crash-safe.
//!
//! Before a page's new content is written to the main database file, it's
//! first appended here, to a separate append-only log file, along with a
//! checksum. Only once the log entry is durably on disk (fsync'd) do we
//! consider a write safe — a crash after that point is fully recoverable:
//! `Pager::open` replays ("redoes") any log entries that never made it
//! into the main file.
//!
//! A transaction is a batch of page writes followed by a commit marker.
//! On recovery, only frames belonging to a *complete* transaction (one
//! with a commit marker after it) are replayed; any trailing, uncommitted
//! frames — exactly what a crash mid-write leaves behind — are discarded.
//! This is "redo" recovery, the same core algorithm real databases use.

use crate::pager::{Page, PAGE_SIZE};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

const PAGE_FRAME_TAG: u8 = 1;
const COMMIT_FRAME_TAG: u8 = 2;

/// tag(1) + page_no(8) + page data(PAGE_SIZE) + checksum(4)
const PAGE_FRAME_SIZE: usize = 1 + 8 + PAGE_SIZE + 4;

pub struct Wal {
    file: File,
}

impl Wal {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).open(path)?;
        Ok(Wal { file })
    }

    pub fn append_page_frame(&mut self, page_no: u64, page: &Page) -> io::Result<()> {
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&[PAGE_FRAME_TAG])?;
        self.file.write_all(&page_no.to_le_bytes())?;
        self.file.write_all(page.as_bytes())?;
        self.file.write_all(&checksum(page.as_bytes()).to_le_bytes())?;
        Ok(())
    }

    pub fn append_commit_frame(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&[COMMIT_FRAME_TAG])?;
        Ok(())
    }

    /// Force the log's contents to physical disk. This is the actual
    /// durability point: once this returns, a crash can no longer lose
    /// the frames written so far.
    pub fn fsync(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }

    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Discard log contents from `len` onward — used to fully empty the
    /// log after a successful checkpoint (len=0), or to roll back an
    /// aborted transaction back to where it started.
    pub fn truncate_to(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Scan the log from the start and return every page write that
    /// belongs to a *complete* transaction. Stops at the first sign of
    /// trouble — a truncated frame, a checksum mismatch, or an
    /// unrecognized tag — since all of those mean "this is where a crash
    /// interrupted a write," and everything from that point on (which was
    /// never confirmed complete) must be discarded, not guessed at.
    pub fn read_committed_frames(&mut self) -> io::Result<Vec<(u64, Page)>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;

        let mut committed = Vec::new();
        let mut pending_batch = Vec::new();
        let mut pos = 0usize;

        while pos < bytes.len() {
            match bytes[pos] {
                PAGE_FRAME_TAG => {
                    if pos + PAGE_FRAME_SIZE > bytes.len() {
                        break; // truncated frame at the tail
                    }
                    let page_no_start = pos + 1;
                    let data_start = page_no_start + 8;
                    let checksum_start = data_start + PAGE_SIZE;

                    let page_no = u64::from_le_bytes(bytes[page_no_start..data_start].try_into().unwrap());
                    let data = &bytes[data_start..checksum_start];
                    let stored_checksum =
                        u32::from_le_bytes(bytes[checksum_start..checksum_start + 4].try_into().unwrap());

                    if checksum(data) != stored_checksum {
                        break; // corrupted/torn frame
                    }

                    let mut page = Page::new();
                    page.as_bytes_mut().copy_from_slice(data);
                    pending_batch.push((page_no, page));
                    pos += PAGE_FRAME_SIZE;
                }
                COMMIT_FRAME_TAG => {
                    committed.append(&mut pending_batch);
                    pos += 1;
                }
                _ => break, // unrecognized tag: treat as corruption, stop here
            }
        }

        // Anything left in `pending_batch` belongs to a transaction that
        // was never confirmed complete — discard it silently.
        Ok(committed)
    }
}

/// A simple polynomial hash (the same technique behind Java's
/// `String.hashCode`) — good enough to catch a torn or corrupted write
/// for this project. Production systems use CRC32 or similar.
fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, &b| acc.wrapping_mul(31).wrapping_add(b as u32))
}
