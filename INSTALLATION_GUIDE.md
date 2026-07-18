# Installation Guide

How to get `docdb`'s interactive shell running on your own machine —
from a clean system with nothing installed, to typing commands at the
prompt.

## 1. Install Rust

You need the Rust toolchain (`rustc` + `cargo`). If you already have it,
skip to [Step 2](#2-clone-the-repository).

**macOS / Linux**

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Follow the prompts (the default install option is fine), then reload
your shell:

```bash
source "$HOME/.cargo/env"
```

**Windows**

Download and run [rustup-init.exe](https://rustup.rs), accept the
defaults. If you don't already have the Microsoft C++ Build Tools, the
installer will prompt you to get them (needed for linking) — follow that
link and install the "Desktop development with C++" workload, then
re-run rustup-init.

Alternatively, on Windows 10/11 you can use WSL (Windows Subsystem for
Linux) and follow the Linux instructions above inside it.

**Verify the install** (any OS):

```bash
rustc --version
cargo --version
```

Both should print a version number. This project was built and tested
against Rust 1.75 but should work on any reasonably recent stable
release.

## 2. Clone the repository

```bash
git clone https://github.com/JaySPX05/Database-Engine.git
cd Database-Engine
```

(No Git? Install it from [git-scm.com](https://git-scm.com/downloads),
or download the repo as a ZIP from GitHub's "Code" button and extract
it, then `cd` into the extracted folder.)

## 3. Build it

```bash
cargo build --release
```

First build takes a minute or two (compiling everything from scratch,
plus Rust's standard library machinery). `--release` produces an
optimized binary — leave it off (`cargo build`) if you're planning to
modify the code and want faster rebuild times instead of a faster
program.

## 4. Run the shell

The simplest way, from inside the project folder, any time:

```bash
cargo run --release
```

This launches straight into the interactive shell:

```
docdb interactive shell. Type 'help' for commands, 'exit' to quit.
(no collection)>
```

### Using it

```
(no collection)> use people
using collection 'people'
people> insert {"name": "Alice", "age": 30}
inserted _id = 0000000000000000
people> find {"age": {"$gt": 25}}
{"name": "Alice", "age": 30, "_id": "0000000000000000"}
people> exit
bye!
```

Full command reference:

| Command | What it does |
|---|---|
| `use <name>` | Switch to (creating if needed) a collection |
| `insert <json>` | Insert a document, e.g. `insert {"name": "Bob"}` |
| `find [json]` | Find documents matching a filter; omit for all documents |
| `get <id>` | Fetch a document by its `_id` |
| `update <id> <json>` | Replace a document's contents (keeps its `_id`) |
| `delete <id>` | Remove a document by its `_id` |
| `help` | Show the command list |
| `exit` / `quit` | Leave the shell |

Data is saved in `<name>.heap.db` and `<name>.index.db` files, created in
whatever directory you ran `cargo run` from. Close the shell and reopen
it (`use` the same collection name) to pick up right where you left off
— everything persists to disk.

There's also a non-interactive walkthrough of every layer of the engine:

```bash
cargo run --release -- demo
```

## 5. (Optional) Install it as a standalone command

If you'd rather type `docdb` from anywhere instead of `cargo run` from
inside the project folder:

```bash
cargo install --path .
```

This builds an optimized binary and copies it to `~/.cargo/bin` (on
Windows, `%USERPROFILE%\.cargo\bin`) — which `rustup` should already have
added to your `PATH`. Once installed:

```bash
docdb
```

works from any directory (documents get saved relative to wherever you
run it from). To remove it later:

```bash
cargo uninstall docdb
```

## Troubleshooting

**`cargo: command not found` after installing Rust**
Your shell hasn't picked up the new `PATH` yet. Close and reopen your
terminal, or run `source "$HOME/.cargo/env"` (macOS/Linux) again.

**Linker errors on Windows during build**
You're missing the C++ Build Tools. Re-run `rustup-init.exe` — it
detects this and offers to install them, or get them directly from
[visualstudio.microsoft.com/visual-cpp-build-tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
(select "Desktop development with C++").

**`docdb` command not found after `cargo install`**
`~/.cargo/bin` isn't on your `PATH`. Add it manually — for bash/zsh, add
this to `~/.bashrc` or `~/.zshrc`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

then restart your terminal.

**Permission denied errors when running the shell**
The database files are created in your current directory — make sure
you have write permission there, or `cd` somewhere you do (like your
home directory) before running `docdb`.

**Build fails with an outdated Rust version**
Run `rustup update` to get the latest stable toolchain, then try
building again.

## Uninstalling everything

```bash
cargo uninstall docdb        # if you did Step 5
rm -rf ~/.cargo ~/.rustup    # removes Rust itself (macOS/Linux)
```

On Windows, uninstall Rust via "Add or Remove Programs," or run
`rustup self uninstall`.
