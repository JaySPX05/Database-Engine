// `mod document;` tells Rust "there's a module in document.rs, compile it
// as part of this crate." Without this line, document.rs would just be an
// inert file Rust never looks at.
mod document;
mod encoding;
mod heap;
mod pager;

use document::{Document, Value};
use heap::HeapFile;

fn main() {
    println!("--- Phase 4: heap file ---");
    let mut heap = HeapFile::open("docdb_heap.db").expect("failed to open heap file");

    let mut small_doc = Document::new();
    small_doc.insert("name", "Alice");
    small_doc.insert("age", 30);
    let small_id = heap.insert(&small_doc).expect("insert failed");
    println!("Inserted small doc at record {:?}", small_id);

    let big_array: Vec<Value> = (0..1500).map(Value::Int).collect();
    let mut big_doc = Document::new();
    big_doc.insert("numbers", big_array);
    let big_id = heap.insert(&big_doc).expect("insert failed");
    println!("Inserted large (multi-page) doc at record {:?}", big_id);

    heap.flush().expect("flush failed");

    let fetched_small = heap.get(small_id).expect("get failed");
    let fetched_big = heap.get(big_id).expect("get failed");
    println!("Small doc round trip OK? {}", fetched_small == small_doc);
    println!("Large doc round trip OK? {}", fetched_big == big_doc);

    heap.delete(small_id).expect("delete failed");
    println!("Deleted small doc's record.");

    let mut new_doc = Document::new();
    new_doc.insert("val", "reused-page-check");
    let new_id = heap.insert(&new_doc).expect("insert failed");
    println!(
        "New insert landed at record {:?} (freed page reused: {})",
        new_id,
        new_id.0 == small_id.0
    );
}
