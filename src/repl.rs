//! An interactive command shell for docdb, so the database can be driven
//! from a terminal instead of editing main.rs and recompiling.
//!
//! Commands:
//!   use <name>              switch to (creating if needed) a collection
//!   insert <json>            insert a document into the current collection
//!   find [json]              find documents matching a query (omit for all)
//!   get <id>                 fetch a document by _id
//!   update <id> <json>       replace a document's contents, keeping its _id
//!   delete <id>              remove a document by _id
//!   help                     show this message
//!   exit | quit              leave the shell

use crate::collection::Collection;
use crate::document::Document;
use crate::json;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};

pub fn run() {
    println!("docdb interactive shell. Type 'help' for commands, 'exit' to quit.");

    let mut collections: HashMap<String, Collection> = HashMap::new();
    let mut current: Option<String> = None;

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    loop {
        print!("{}> ", current.as_deref().unwrap_or("(no collection)"));
        io::stdout().flush().ok();

        let line = match lines.next() {
            Some(Ok(line)) => line,
            // EOF (stdin closed, or piped input ran out) or a read error:
            // either way, there's nothing more to read, so exit cleanly.
            Some(Err(_)) | None => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let (command, rest) = split_first_word(line);

        match command {
            "exit" | "quit" => break,
            "help" => print_help(),
            "use" => handle_use(&mut collections, &mut current, rest),
            "insert" => with_current(&mut collections, &current, |coll| handle_insert(coll, rest)),
            "find" => with_current(&mut collections, &current, |coll| handle_find(coll, rest)),
            "get" => with_current(&mut collections, &current, |coll| handle_get(coll, rest)),
            "update" => with_current(&mut collections, &current, |coll| handle_update(coll, rest)),
            "delete" => with_current(&mut collections, &current, |coll| handle_delete(coll, rest)),
            other => println!("unknown command: '{other}' (type 'help' for a list)"),
        }
    }

    // Flush every collection we touched before exiting, so nothing from
    // this session is lost.
    for (_, mut coll) in collections {
        let _ = coll.flush();
    }
    println!("bye!");
}

fn handle_use(collections: &mut HashMap<String, Collection>, current: &mut Option<String>, rest: &str) {
    let name = rest.trim();
    if name.is_empty() {
        println!("usage: use <collection name>");
        return;
    }
    if !collections.contains_key(name) {
        match Collection::open(name) {
            Ok(coll) => {
                collections.insert(name.to_string(), coll);
            }
            Err(e) => {
                println!("error opening collection '{name}': {e}");
                return;
            }
        }
    }
    *current = Some(name.to_string());
    println!("using collection '{name}'");
}

fn handle_insert(coll: &mut Collection, rest: &str) {
    match json::parse_document(rest) {
        Ok(doc) => match coll.insert(doc) {
            Ok(id) => println!("inserted _id = {id}"),
            Err(e) => println!("error: {e}"),
        },
        Err(e) => println!("parse error: {e}"),
    }
}

fn handle_find(coll: &mut Collection, rest: &str) {
    let query_text = rest.trim();
    // An empty filter matches everything (`find` with no arguments), same
    // convention as Mongo's `db.collection.find()`.
    let query = if query_text.is_empty() { Ok(Document::new()) } else { json::parse_document(query_text) };

    match query {
        Ok(q) => match coll.find(&q) {
            Ok(docs) if docs.is_empty() => println!("(no matches)"),
            Ok(docs) => {
                for doc in docs {
                    println!("{}", json::to_json_string(&doc));
                }
            }
            Err(e) => println!("error: {e}"),
        },
        Err(e) => println!("parse error: {e}"),
    }
}

fn handle_get(coll: &mut Collection, rest: &str) {
    let id = rest.trim();
    if id.is_empty() {
        println!("usage: get <id>");
        return;
    }
    match coll.find_by_id(id) {
        Ok(Some(doc)) => println!("{}", json::to_json_string(&doc)),
        Ok(None) => println!("(not found)"),
        Err(e) => println!("error: {e}"),
    }
}

fn handle_update(coll: &mut Collection, rest: &str) {
    let (id, json_text) = split_first_word(rest);
    if id.is_empty() || json_text.trim().is_empty() {
        println!("usage: update <id> <json>");
        return;
    }
    match json::parse_document(json_text) {
        Ok(doc) => match coll.update_by_id(id, doc) {
            Ok(true) => println!("updated"),
            Ok(false) => println!("(not found)"),
            Err(e) => println!("error: {e}"),
        },
        Err(e) => println!("parse error: {e}"),
    }
}

fn handle_delete(coll: &mut Collection, rest: &str) {
    let id = rest.trim();
    if id.is_empty() {
        println!("usage: delete <id>");
        return;
    }
    match coll.delete_by_id(id) {
        Ok(true) => println!("deleted"),
        Ok(false) => println!("(not found)"),
        Err(e) => println!("error: {e}"),
    }
}

/// Look up the currently selected collection and run `f` against it, or
/// print a helpful message if no collection has been selected yet.
fn with_current(collections: &mut HashMap<String, Collection>, current: &Option<String>, f: impl FnOnce(&mut Collection)) {
    match current {
        None => println!("no collection selected — try 'use <name>' first"),
        Some(name) => match collections.get_mut(name) {
            Some(coll) => f(coll),
            None => println!("internal error: collection '{name}' not open"),
        },
    }
}

/// Split `s` into its first whitespace-delimited word and the (trimmed)
/// remainder. Used to peel off command keywords and, for `update`, the
/// id before the JSON body.
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], s[idx..].trim_start()),
        None => (s, ""),
    }
}

fn print_help() {
    println!(
        "Commands:
  use <name>            switch to (creating if needed) a collection
  insert <json>          insert a document, e.g. insert {{\"name\": \"Alice\", \"age\": 30}}
  find [json]            find documents matching a query; omit for all documents
                          e.g. find {{\"age\": {{\"$gt\": 25}}}}
  get <id>               fetch a document by its _id
  update <id> <json>     replace a document's contents (keeps its _id)
  delete <id>            remove a document by its _id
  help                   show this message
  exit | quit            leave the shell"
    );
}
