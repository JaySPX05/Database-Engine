# CLI Guide

How to get `docdb` running on your own machine and drive it from your
terminal. This assumes no prior Rust setup — if you already have Rust
installed, skip to [Get the code](#2-get-the-code).

## 1. Install Rust

`docdb` needs the Rust toolchain (`cargo` + `rustc`). If you don't have
it yet:

**macOS / Linux**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```
Follow the prompts, then restart your terminal (or run the `source`
command it prints) so `cargo` is on your `PATH`.

**Windows**
Download and run [rustup-init.exe](https://rustup.rs) and follow the
installer prompts. Use PowerShell or Command Prompt for the commands
below.

**Check it worked:**
```bash
cargo --version
rustc --version
```
Any reasonably recent version works — this project was built and tested
on Rust 1.75.

## 2. Get the code

```bash
git clone https://github.com/JaySPX05/Database-Engine.git
cd Database-Engine
```

## 3. Build it

```bash
cargo build
```

First build takes a little while (compiling dependencies + the project).
Later builds are fast — Cargo only rebuilds what changed.

Optional: confirm everything works before you rely on it —
```bash
cargo test
```
should report `46 passed; 0 failed`.

## 4. Run the shell

```bash
cargo run
```

You'll land in the interactive prompt:
```
docdb interactive shell. Type 'help' for commands, 'exit' to quit.
(no collection)>
```

Everything from here happens by typing commands.

## 5. Command reference

| Command | What it does | Example |
|---|---|---|
| `use <name>` | Switch to a collection, creating it if it doesn't exist yet | `use people` |
| `insert <json>` | Insert a document | `insert {"name": "Alice", "age": 30}` |
| `find [json]` | Find documents matching a filter; omit the filter to list everything | `find {"age": {"$gt": 25}}` |
| `get <id>` | Fetch one document by its `_id` | `get 0000000000000000` |
| `update <id> <json>` | Replace a document's contents (keeps its `_id`) | `update 0000000000000000 {"name": "Alice", "age": 31}` |
| `delete <id>` | Remove a document by its `_id` | `delete 0000000000000000` |
| `help` | Show the command list | `help` |
| `exit` / `quit` | Leave the shell | `exit` |

You must `use` a collection before `insert`/`find`/`get`/`update`/`delete`
will work — the shell will remind you if you forget.

### A full walkthrough

```
(no collection)> use people
using collection 'people'
people> insert {"name": "Alice", "age": 30}
inserted _id = 0000000000000000
people> insert {"name": "Bob", "age": 25}
inserted _id = 0000000000000001
people> insert {"name": "Carol", "age": 45, "tags": ["vip", "founder"]}
inserted _id = 0000000000000002
people> find
{"name": "Alice", "age": 30, "_id": "0000000000000000"}
{"name": "Bob", "age": 25, "_id": "0000000000000001"}
{"name": "Carol", "age": 45, "tags": ["vip", "founder"], "_id": "0000000000000002"}
people> find {"age": {"$gt": 26}}
{"name": "Alice", "age": 30, "_id": "0000000000000000"}
{"name": "Carol", "age": 45, "tags": ["vip", "founder"], "_id": "0000000000000002"}
people> update 0000000000000001 {"name": "Bob", "age": 26}
updated
people> delete 0000000000000002
deleted
people> find
{"name": "Alice", "age": 30, "_id": "0000000000000000"}
{"name": "Bob", "age": 26, "_id": "0000000000000001"}
people> exit
bye!
```

### Query filter syntax

Filters are JSON objects. A plain value means equality; a nested object
with a `$`-prefixed key means "apply this operator":

```
find {"name": "Alice"}                        equality
find {"age": {"$gt": 25}}                      greater than
find {"age": {"$gte": 18, "$lte": 65}}         range (both must hold)
find {"name": {"$in": ["Alice", "Bob"]}}       membership
find {"email": {"$exists": false}}             field absent
find {"$or": [{"age": {"$lt": 20}}, {"name": "Carol"}]}   either condition
```

Supported operators: `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`, `$in`,
`$nin`, `$exists`, plus the combinators `$and` / `$or`.

## 6. Working with multiple collections

You can `use` more than one collection in the same session — each stays
open, and `use` just switches which one subsequent commands apply to:

```
(no collection)> use people
using collection 'people'
people> insert {"name": "Alice"}
inserted _id = 0000000000000000
people> use products
using collection 'products'
products> insert {"title": "Keyboard", "price": 49.99}
inserted _id = 0000000000000000
products> use people
using collection 'people'
people> find
{"name": "Alice", "_id": "0000000000000000"}
```

Note `_id` counters are per-collection, so it's normal for two different
collections to both hand out `0000000000000000` as their first id.

## 7. Where your data lives

Each collection named `<name>` creates two files in the directory you ran
`cargo run` from:

- `<name>.heap.db` — the actual document data
- `<name>.index.db` — the `_id` index

Plus, briefly during writes, `<name>.heap.db-wal` and
`<name>.index.db-wal` — the write-ahead logs. These are normally empty
between commands (each write commits and clears its log immediately) and
exist mainly to protect a write that's in progress if the process is
killed mid-write.

**To pick up where you left off**, just run `cargo run` again from the
same directory and `use` the same collection name — the data is already
there.

**To start completely fresh**, delete the `.db` / `.db-wal` files (or run
from a different directory).

## 8. Other ways to run it

```bash
cargo run -- demo     # replays a scripted tour of every layer, non-interactive
cargo test             # run the full test suite (46 tests)
cargo build --release  # optimized build; binary at target/release/docdb
```

The release binary runs noticeably faster and can be copied anywhere and
run directly (`./target/release/docdb`) without `cargo` in the loop.

## Troubleshooting

**`cargo: command not found`** — Rust isn't installed, or your terminal
hasn't picked up the updated `PATH` yet. Restart your terminal, or run
the `source $HOME/.cargo/env` line the rustup installer printed.

**Data doesn't seem to persist between runs** — you're probably running
`cargo run` from a different directory each time. The `.db` files are
created relative to your current working directory; `cd` into the same
folder before running again.

**`parse error: ...` after typing a command** — the JSON you typed
wasn't valid. Common causes: single quotes instead of double quotes
around strings, a trailing comma before `}` or `]`, or an unquoted key.
JSON requires double-quoted keys and strings: `{"age": 30}`, not
`{age: 30}`.

**Want to inspect the raw files?** They're binary (not human-readable) —
see [`ARCHITECTURE.md`](./ARCHITECTURE.md) for the exact on-disk formats
if you want to understand what's actually in them.
