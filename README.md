# docdb

A document database, built from scratch in Rust — no storage engine
crates, no SQL engine, no JSON library. Every layer, from raw bytes on
disk up to a MongoDB-style query language and an interactive shell, is
implemented here.

This project exists to learn how databases actually work, not to compete
with real ones. If you're reading the source to understand the *ideas*
(pages, B-Trees, write-ahead logs, query matching), this README and
[`ARCHITECTURE.md`](./ARCHITECTURE.md) are your map.

## What it can do

- Store JSON-like documents (strings, numbers, booleans, nested objects,
  arrays) persistently on disk
- Look up a document by `_id` in O(log n) via a B+Tree index
- Query with MongoDB-style filters: `{"age": {"$gt": 25}}`, `$and`, `$or`,
  `$in`, `$exists`, etc.
- Survive a crash mid-write without corruption, via a write-ahead log
- Talk to it through an interactive shell (`cargo run`) using real JSON

## What it deliberately doesn't do

This is a teaching project, and some corners are cut on purpose — each is
called out in code comments where it matters:

- **No B-Tree deletion rebalancing.** Deleting a key removes it from its
  leaf but never merges underflowed nodes. Costs some wasted space over
  time; never causes incorrect results.
- **No secondary indexes.** Only `_id` is indexed. `find()` with any other
  filter does a full collection scan.
- **No concurrency.** One process, one thread, at a time.
- **No cross-file transactions.** A `Collection`'s heap file and index
  file each get their own crash-safe transactions, but an operation
  spanning both (like `update_by_id`) isn't atomic *across* the two files
  — a crash between them can leave a harmless orphaned page, never
  corruption.

## Quick start

```bash
git clone https://github.com/JaySPX05/Database-Engine.git
cd Database-Engine
cargo build
cargo test      # 46 tests, every layer
cargo run       # launches the interactive shell
```

### Using the shell

```
docdb interactive shell. Type 'help' for commands, 'exit' to quit.
(no collection)> use people
using collection 'people'
people> insert {"name": "Alice", "age": 30}
inserted _id = 0000000000000000
people> insert {"name": "Bob", "age": 25}
inserted _id = 0000000000000001
people> find {"age": {"$gt": 26}}
{"name": "Alice", "age": 30, "_id": "0000000000000000"}
people> get 0000000000000001
{"name": "Bob", "age": 25, "_id": "0000000000000001"}
people> update 0000000000000001 {"name": "Bob", "age": 26}
updated
people> delete 0000000000000001
deleted
people> exit
bye!
```

Commands: `use <name>`, `insert <json>`, `find [json]`, `get <id>`,
`update <id> <json>`, `delete <id>`, `help`, `exit`/`quit`.

Data persists in `<name>.heap.db` / `<name>.index.db` files in the
working directory — reopen the shell and `use` the same name to pick up
where you left off.

There's also `cargo run -- demo`, which replays a scripted walkthrough of
every layer (heap file, B-Tree, queries, crash recovery) in one shot —
useful for seeing the whole system exercised without typing anything.

## How a request flows through the system

Inserting a document touches every layer, top to bottom:

```
Collection::insert(doc)
  │
  ├─ generates an _id if the doc doesn't have one
  │
  ├─ HeapFile::insert(doc)          "where do the bytes live?"
  │    ├─ Document::to_bytes()      (encoding.rs: Document -> Vec<u8>)
  │    ├─ splits bytes across pages if the document is large
  │    └─ Pager::write_page(...)    for each page in the chain
  │         ├─ wraps writes in a transaction (all pages, or none)
  │         └─ WAL::append_page_frame + fsync, then applies to the file
  │
  └─ BTree::insert(_id, RecordId)   "how do I find it again?"
       ├─ descends to the correct leaf page
       ├─ inserts in sorted order, splitting pages if they overflow
       └─ Pager::write_page(...)    same crash-safe path as above
```

Looking a document up by `_id` runs the index step (fast, O(log n)) to
get a `RecordId`, then a heap fetch (follow the page chain, decode the
bytes). Running a `find()` query currently skips the index and just scans
every document via `Collection::all()`, checking each one against the
filter.

## Project layout

```
src/
├── document.rs   Value/Document — the core data model (an enum + an
│                 ordered Vec<(String, Value)>)
├── encoding.rs    Binary serialization: Document <-> Vec<u8>
├── pager.rs       Fixed 4KB pages on disk + transactions
├── wal.rs         Write-ahead log: the crash-durability mechanism
├── heap.rs        Multi-page document storage + free-space reuse
├── btree.rs       B+Tree index: String key -> RecordId, O(log n)
├── collection.rs  Ties HeapFile + BTree into insert/find_by_id/update/delete
├── query.rs       MongoDB-style filter matching ($gt, $or, ...)
├── json.rs        Hand-written JSON parser (text <-> Document)
├── repl.rs         Interactive shell
└── main.rs        Entry point: launches the shell, or `-- demo`
```

Read them in that order — each one only depends on the ones above it,
which is also the order they were originally built in.

## Testing

```bash
cargo test              # all 46 tests
cargo test heap::        # just one module's tests
cargo test -- --nocapture   # see println! output from tests
```

Every module has unit tests next to its code (`#[cfg(test)] mod tests`).
A few are worth reading even if you don't read the rest of the module:

- `heap::tests::counter_survives_free_list_churn` — a regression test for
  a real bug that was found and fixed during development (see
  [`ARCHITECTURE.md`](./ARCHITECTURE.md#a-real-bug-worth-reading-about)).
- `pager::tests::crash_recovery_replays_committed_but_unapplied_writes` —
  directly proves the WAL's core promise by writing to the log only,
  never touching the main file, and confirming a fresh `Pager::open`
  recovers it anyway.
- `btree::tests::bulk_insert_forces_splits_and_stays_correct` — inserts
  1000 keys out of order and confirms every one is still findable after
  the tree has split repeatedly.

## Further reading

[`ARCHITECTURE.md`](./ARCHITECTURE.md) goes module-by-module into *why*
things are built the way they are — on-disk formats, the B-Tree split
algorithm, the WAL recovery protocol, and the tradeoffs behind each
simplification.
