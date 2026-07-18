// `mod document;` tells Rust "there's a module in document.rs, compile it
// as part of this crate." Without this line, document.rs would just be an
// inert file Rust never looks at.
mod btree;
mod collection;
mod document;
mod encoding;
mod heap;
mod pager;
mod query;
mod wal;

use btree::BTree;
use collection::Collection;
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

    println!("\n--- Phase 5: B-Tree index ---");
    let mut index = BTree::open("docdb_index.db").expect("failed to open index");

    // Index a handful of people by name -> RecordId.
    let people = [
        ("carol", 55u64),
        ("alice", 42u64),
        ("dave", 7u64),
        ("bob", 19u64),
    ];
    for (name, rid) in people {
        index.insert(name, heap::RecordId(rid)).expect("index insert failed");
    }
    index.flush().expect("flush failed");

    println!("Point lookup 'alice' -> {:?}", index.get("alice").unwrap());
    println!("Point lookup 'nobody' -> {:?}", index.get("nobody").unwrap());

    println!("Sorted scan of all indexed keys:");
    for (key, rid) in index.scan_all().unwrap() {
        println!("  {key} -> {rid:?}");
    }

    // Prove the index actually restructures under load: insert enough
    // keys to force real page splits, then confirm every one is still
    // correctly findable afterward.
    for i in 0..2000 {
        index.insert(&format!("bulk{i:05}"), heap::RecordId(i)).unwrap();
    }
    let all_found = (0..2000).all(|i| {
        index.get(&format!("bulk{i:05}")).unwrap() == Some(heap::RecordId(i))
    });
    println!("All 2000 bulk-inserted keys correctly findable after splits? {all_found}");

    println!("\n--- Phase 6: Collection API ---");
    let mut people = Collection::open("docdb_people").expect("failed to open collection");

    let mut alice = Document::new();
    alice.insert("name", "Alice");
    alice.insert("age", 30);
    let alice_id = people.insert(alice).expect("insert failed");
    println!("Inserted Alice with _id = {alice_id}");

    let mut bob = Document::new();
    bob.insert("name", "Bob");
    bob.insert("age", 25);
    let bob_id = people.insert(bob).expect("insert failed");
    println!("Inserted Bob with _id = {bob_id}");

    people.flush().expect("flush failed");

    let fetched = people.find_by_id(&alice_id).expect("find failed");
    println!("find_by_id(alice) -> {fetched:?}");

    let mut updated_alice = Document::new();
    updated_alice.insert("name", "Alice");
    updated_alice.insert("age", 31); // had a birthday
    people.update_by_id(&alice_id, updated_alice).expect("update failed");
    let after_update = people.find_by_id(&alice_id).unwrap().unwrap();
    println!("After update, Alice's age = {:?}", after_update.get("age"));

    println!("All people in the collection:");
    for doc in people.all().unwrap() {
        println!("  {doc:?}");
    }

    people.delete_by_id(&bob_id).expect("delete failed");
    println!(
        "Deleted Bob. find_by_id(bob) now returns: {:?}",
        people.find_by_id(&bob_id).unwrap()
    );

    println!("\n--- Phase 7: query engine ---");
    let mut carol = Document::new();
    carol.insert("name", "Carol");
    carol.insert("age", 45);
    people.insert(carol).expect("insert failed");

    let mut dave = Document::new();
    dave.insert("name", "Dave");
    dave.insert("age", 19);
    people.insert(dave).expect("insert failed");
    people.flush().expect("flush failed");

    let mut over_30 = Document::new();
    over_30.insert("age", query::gt(30));
    println!("People with age > 30:");
    for doc in people.find(&over_30).unwrap() {
        println!("  {:?} (age {:?})", doc.get("name"), doc.get("age"));
    }

    let mut named_dave = Document::new();
    named_dave.insert("name", "Dave");
    println!("People named Dave:");
    for doc in people.find(&named_dave).unwrap() {
        println!("  {:?}", doc.get("name"));
    }

    let mut young_or_carol = Document::new();
    let mut young = Document::new();
    young.insert("age", query::lt(20));
    let mut named_carol = Document::new();
    named_carol.insert("name", "Carol");
    young_or_carol.insert(
        "$or",
        Value::Array(vec![Value::Document(young), Value::Document(named_carol)]),
    );
    println!("People under 20 OR named Carol:");
    for doc in people.find(&young_or_carol).unwrap() {
        println!("  {:?} (age {:?})", doc.get("name"), doc.get("age"));
    }

    println!("\n--- Phase 8: write-ahead log / crash recovery ---");
    demo_crash_recovery();
}

/// Simulates a crash by writing directly to the WAL (bypassing the main
/// data file entirely, exactly like a process that died right after the
/// WAL was fsync'd but before the main file was updated), then opens a
/// fresh Pager and shows it recovering that write automatically.
fn demo_crash_recovery() {
    use pager::{Page, Pager};
    use wal::Wal;

    let db_path = "docdb_crash_demo.db";
    let wal_path = format!("{db_path}-wal");
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(&wal_path);

    {
        let mut wal = Wal::open(&wal_path).expect("failed to open wal");
        let message = b"I survived a simulated crash";
        let mut page = Page::new();
        page.as_bytes_mut()[..message.len()].copy_from_slice(message);
        wal.append_page_frame(2, &page).expect("wal write failed");
        wal.append_commit_frame().expect("wal commit failed");
        wal.fsync().expect("wal fsync failed");
        println!("Wrote a committed frame to the WAL only — the main .db file was never touched.");
    }

    println!("Opening the database fresh, as if this were a brand-new process after a crash...");
    let mut pager = Pager::open(db_path).expect("pager open (with recovery) failed");
    let recovered = pager.read_page(2).expect("failed to read recovered page");
    let message_len = "I survived a simulated crash".len();
    let text = std::str::from_utf8(&recovered.as_bytes()[..message_len]).unwrap();
    println!("Recovered page 2 contents: {text:?}");
    println!("Recovery worked — the committed write survived even though it never reached the main file before we \"crashed\".");
}
