// `mod document;` tells Rust "there's a module in document.rs, compile it
// as part of this crate." Without this line, document.rs would just be an
// inert file Rust never looks at.
mod document;
mod encoding;
mod pager;

use document::Document;
use pager::{Page, Pager};

fn main() {
    let mut doc = Document::new();
    doc.insert("name", "Alice");
    doc.insert("age", 30);
    doc.insert("active", true);

    println!("Document has {} fields", doc.len());
    for (key, value) in doc.iter() {
        println!("  {key}: {value:?}");
    }

    let bytes = doc.to_bytes();
    println!("\nEncoded to {} bytes", bytes.len());

    let decoded = Document::from_bytes(&bytes).expect("decode should succeed");
    println!("Round trip equal? {}", decoded == doc);

    // --- Phase 3: actually persist those bytes to disk ---
    println!("\n--- Writing to disk via the Pager ---");
    let mut pager = Pager::open("docdb_data.db").expect("failed to open db file");
    println!("Existing page count on open: {}", pager.page_count());

    let page_no = pager.allocate_page().expect("failed to allocate page");
    let mut page = Page::new();
    // A page is PAGE_SIZE bytes; our encoded document is much smaller,
    // so it just occupies the front of the page and the rest stays zero.
    page.as_bytes_mut()[..bytes.len()].copy_from_slice(&bytes);
    pager.write_page(page_no, &page).expect("failed to write page");
    pager.flush().expect("failed to flush to disk");
    println!("Wrote document to page {page_no}, flushed to disk.");

    let read_page = pager.read_page(page_no).expect("failed to read page");
    let read_back = Document::from_bytes(&read_page.as_bytes()[..bytes.len()])
        .expect("failed to decode page contents");
    println!("Read back from disk: {read_back:?}");
    println!("Matches original? {}", read_back == doc);
}
