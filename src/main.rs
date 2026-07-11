// `mod document;` tells Rust "there's a module in document.rs, compile it
// as part of this crate." Without this line, document.rs would just be an
// inert file Rust never looks at.
mod document;
mod encoding;

use document::Document;

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
    println!("\nEncoded to {} bytes: {:?}", bytes.len(), bytes);

    let decoded = Document::from_bytes(&bytes).expect("decode should succeed");
    println!("\nDecoded back: {decoded:?}");
    println!("Round trip equal? {}", decoded == doc);
}
