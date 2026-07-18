# Architecture

A module-by-module walkthrough of how `docdb` works and why it's built
this way. Each section maps to one file in `src/` and assumes you've read
the previous ones — the layers build on each other in this exact order.

## Table of contents

1. [document.rs — the data model](#documentrs--the-data-model)
2. [encoding.rs — binary serialization](#encodingrs--binary-serialization)
3. [pager.rs — pages and transactions](#pagerrs--pages-and-transactions)
4. [wal.rs — the write-ahead log](#walrs--the-write-ahead-log)
5. [heap.rs — document storage](#heaprs--document-storage)
6. [btree.rs — the index](#btreers--the-index)
7. [collection.rs — tying it together](#collectionrs--tying-it-together)
8. [query.rs — the query engine](#queryrs--the-query-engine)
9. [json.rs and repl.rs — the shell](#jsonrs-and-replrs--the-shell)
10. [A real bug worth reading about](#a-real-bug-worth-reading-about)

---

## document.rs — the data model

Two types: `Value`, an enum covering every kind of data a field can hold
(`Null`, `Bool`, `Int`, `Float`, `Str`, `Array`, `Document`), and
`Document`, an ordered list of `(String, Value)` pairs.

**Why `Vec<(String, Value)>` instead of `HashMap`?** Two reasons: real
BSON documents preserve field insertion order and a `HashMap` doesn't,
and documents are small enough (a handful of fields) that a linear scan
beats hashing in practice anyway.

`Value::Document` nests without needing `Box` — `Document` already
contains a `Vec`, which is heap-allocated, so there's no infinite-size
problem for the compiler to reject.

---

## encoding.rs — binary serialization

Turns a `Document` into `Vec<u8>` and back. The format is TLV-style
(tag-length-value): every `Value` starts with a 1-byte tag identifying
its variant, followed by variant-specific payload.

| Variant | Tag | Payload |
|---|---|---|
| Null | 0 | *(none)* |
| Bool | 1 | 1 byte |
| Int | 2 | 8 bytes, i64 little-endian |
| Float | 3 | 8 bytes, f64 little-endian |
| Str | 4 | 4-byte length + UTF-8 bytes |
| Array | 5 | 4-byte count + each Value back to back |
| Document | 6 | 4-byte field count + each (key, value) pair |

Decoding walks a `Cursor` (a byte slice + a position) and returns
`Result<_, DecodeError>` at every step — a truncated or corrupted buffer
produces a clean error, never a panic or an out-of-bounds read. This
matters because these bytes eventually come from disk, and disks can
hold corrupted or partial data after a crash.

---

## pager.rs — pages and transactions

Everything above this layer works in page numbers, never raw file
offsets. A **page** is a fixed 4096-byte chunk; page number `n` always
lives at byte offset `n * 4096` in the file. This gives O(1) random
access and matches common OS/filesystem block sizes, so one page I/O
tends to map to one physical disk I/O — the same choice SQLite makes.

Since Phase 8, `Pager` also owns transaction semantics:

- **Auto-commit** (default): a bare `write_page()` call is its own
  one-page transaction — logged, fsync'd, applied, checkpointed,
  immediately. Every write is crash-safe without the caller doing
  anything extra.
- **Explicit transactions** (`begin_transaction` / `commit_transaction` /
  `rollback_transaction`): for operations that touch several pages which
  must succeed or fail *together*. Writes are buffered in memory
  (`pending: Vec<(u64, Page)>`) and logged to the WAL as they happen, but
  not applied to the main file until commit.
- **Read-your-own-writes**: `read_page()` checks the pending buffer
  before falling back to the file, so code inside a transaction sees its
  own uncommitted changes immediately (needed, for example, when the
  heap file's free-list logic reads-modifies-writes the same metadata
  page multiple times within one transaction).

See `wal.rs` for what "logged" actually means and why it's the source of
the crash-safety guarantee.

---

## wal.rs — the write-ahead log

The core rule: **never modify the real database file before the
intended change is durably logged elsewhere.** If a crash happens before
the log write finishes, the real file was never touched — nothing to
recover. If a crash happens after the log is durable but before the real
file is updated, the log still has the information needed to finish the
job; that's exactly what `Pager::open()`'s recovery step does on every
startup.

**Log format**: a sequence of frames.
- A **page frame**: `[tag=1][page_no: u64][page data: 4096 bytes][checksum: u32]`
- A **commit frame**: `[tag=2]` — marks "everything since the last commit
  is a complete transaction."

**Recovery** (`read_committed_frames`) scans from the start, buffering
page frames until it hits a commit marker, at which point that whole
batch moves into the "will be replayed" list. The moment it hits
anything abnormal — a truncated frame, a bad checksum, an unrecognized
tag — it stops immediately and discards whatever was still buffered.
That's not a bug; it's the point. A crash mid-write leaves exactly this
kind of trailing, incomplete data, and the only safe thing to do with an
unconfirmed transaction is throw it away.

```
[page frame][page frame][commit] [page frame][page frame]
└──────── replayed ────────┘     └── discarded: no commit followed ──┘
```

The checksum is a simple polynomial hash (same idea as Java's
`String.hashCode`) — good enough to catch a torn write for this project.
Production databases use CRC32 or stronger.

---

## heap.rs — document storage

Stores documents of arbitrary size as a chain of linked pages. Each page
in a chain has a 12-byte header — `next_page: u64` (or a `NONE` sentinel
for "this is the last page") and `payload_len: u32` — followed by up to
~4084 bytes of the document's encoded bytes. A `RecordId` is just the
page number where a document's chain begins.

**Free list**: deleting a document doesn't just abandon its pages — they
get pushed onto a linked list of reusable pages, with a clever bit of
reuse: a freed page repurposes its own first 8 bytes to point at the
*next* free page. No extra storage needed for the free list itself.
`insert()`'s page allocator checks this list before asking the pager to
grow the file.

**The `_id` counter**: lives at byte offset 8 of the heap's metadata page
(page 0), right after the free-list head at offset 0. `next_counter_value()`
reads-and-increments it. `Collection` uses this to mint `_id` values
without a UUID dependency.

Both `insert()` (writing a multi-page chain) and `delete()` (freeing one)
wrap their page writes in an explicit `Pager` transaction — a crash
partway through either would otherwise leave a half-written chain or a
corrupted free list.

---

## btree.rs — the index

A disk-backed **B+Tree**: `String` keys mapped to `RecordId`s, kept
sorted, searchable in O(log n).

- **Leaf nodes** hold sorted `(key, RecordId)` pairs and a pointer to the
  next leaf — leaves are chained left-to-right, so an in-order scan
  (`scan_all()`) never has to re-descend the tree, just walk sideways.
- **Internal nodes** hold N separator keys and N+1 child page pointers.
  To route a search key, find the first separator greater than it and
  descend the child just before that position.

**Insertion**: decode the target page into an owned `Node` enum, mutate
it in memory, and — only if that mutation makes the encoded page exceed
4096 bytes — split it into two pages and return the new page's first key
to the caller (parent), which repeats the same "insert, check overflow,
maybe split" logic one level up. If the split propagates all the way to
the root, a *brand new* root is created above the old one — the only way
the tree grows taller.

```
        [ root splits ]
             │
      ┌──────┴──────┐
   [ left ]      [ right ]      <- new root now points to both
      │
  (insert cascades from leaf upward only as far as needed)
```

The whole recursive insert (however many levels it touches) is wrapped
in one `Pager` transaction, so a cascading multi-level split is
all-or-nothing.

**Known simplification**: `delete()` removes a key from its leaf but
never merges underflowed nodes with siblings. This wastes some space
after heavy deletion but never produces wrong answers — every remaining
key is still exactly where the tree's invariants say it should be.

---

## collection.rs — tying it together

`Collection` owns a `HeapFile` and a `BTree` and coordinates between
them — this is the layer application code actually calls.

```
insert(doc)      -> assign _id if missing -> heap.insert -> index.insert(_id, RecordId)
find_by_id(id)    -> index.get(id) -> heap.get(RecordId)
update_by_id(...)  -> heap.delete(old) -> heap.insert(new) -> index.insert(id, new RecordId)
delete_by_id(id)   -> index.get(id) -> heap.delete -> index.delete
all()             -> index.scan_all() -> heap.get() for each   (sorted by _id, "for free")
```

`update_by_id` is delete-then-reinsert rather than an in-place edit,
since the new content may be a different size and land on entirely
different pages. The index is exactly what makes this invisible to
callers — `id` keeps resolving correctly no matter where the physical
bytes moved to.

**Known limitation**: because the heap file and index file are two
separate `Pager`s (two separate WALs), an operation spanning both isn't
atomic as one unit — e.g. a crash between `heap.insert()` succeeding and
`index.insert()` running in `Collection::insert()` leaves an orphaned,
harmless heap page with no index entry pointing to it. Fixing this
properly would mean a shared WAL or two-phase commit across files —
a real extension, left out here to keep the transaction model
approachable.

---

## query.rs — the query engine

A query is just a `Document` — `matches(doc, query)` decides whether a
document satisfies it.

- A plain value under a field means equality: `{"name": "Alice"}`.
- A nested document whose keys start with `$` means "apply these
  operators, all must pass": `{"age": {"$gt": 25, "$lt": 65}}`.
- `$and` / `$or` take an array of sub-queries and recurse into `matches`
  again — the same recursive shape used for nested documents from the
  very first module.

Supported operators: `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`, `$in`,
`$nin`, `$exists`. Numeric comparison works across `Int`/`Float` (an
`Int` field can be compared against a `Float` query bound and vice
versa); comparing incompatible types (e.g. a string against a number)
simply never matches rather than erroring.

`Collection::find()` currently scans every document via `all()` and
filters in memory — there's no secondary indexing, so this is O(n)
regardless of the filter.

---

## json.rs and repl.rs — the shell

`json.rs` is a small hand-written recursive-descent parser converting
JSON text to `Document`/`Value` and back — `parse_value_from` looks at
the next character to decide what follows (`{` → object, `"` → string,
digit or `-` → number, ...), and objects/arrays recursively call back
into it for their contents.

`repl.rs` is a straightforward read-eval-print loop: read a line, split
off the first word as a command, dispatch to a handler, print the
result. Multiple collections can be open at once (`use <name>` switches
between them, opening on first reference). Every failure path — bad
JSON, a missing argument, an unknown command — produces a message
instead of panicking, since this is the one layer a human types into
directly.

---

## A real bug worth reading about

While building the query engine (Phase 7), filtering by `{"age": {"$gt": 30}}`
returned *nothing* — even for documents that obviously matched. The
query logic itself was correct; the actual problem was one layer down.

`HeapFile`'s metadata page (page 0) holds two unrelated pieces of state:
the free-list head at byte offset 0, and the `_id` counter at byte offset
8. The bug: both `allocate_page()`'s free-list-reuse branch and
`free_page()` were reconstructing that page from scratch —
`let mut meta = Page::new()` — instead of editing the existing one. Every
time a page got freed or reused, this silently zeroed out the counter.

`Collection::update_by_id` calls `heap.delete()` then `heap.insert()` —
exactly the sequence that triggers both bugged code paths. After one
update, the counter reset to 0, and the next document inserted got the
same `_id` as an *earlier* document, silently overwriting its index
entry. Documents were vanishing from `all()`/`find()` without any error
ever being raised.

**The fix**: clone the existing metadata page and edit only the bytes
that should change, preserving everything else — `heap.rs`'s
`counter_survives_free_list_churn` test guards against a regression.

The lesson generalizes: whenever one page (or file, or struct) holds more
than one independent piece of state, "reset this one field" and
"reconstruct the whole thing from a blank slate" are *not* the same
operation — and the bug they cause won't show up in a unit test that only
exercises one piece of state at a time. It took a full-stack integration
test (insert → update → query) to surface it, even though every
lower-layer unit test had been green the whole time.
