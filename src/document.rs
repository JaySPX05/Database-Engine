//! The core data model: `Value` (a single field's data) and `Document`
//! (an ordered collection of key-value pairs). This is our version of BSON.

/// A single value that can be stored in a document.
///
/// This is an "algebraic data type" (a.k.a. tagged union): a `Value` is
/// ALWAYS exactly one of these variants, never more than one, never zero.
/// The compiler forces you to handle every variant when you pattern-match,
/// which means you can never forget a case (very different from e.g. a
/// JSON `any` type in dynamically typed languages).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Value>),
    // A document can contain another document (nesting), just like BSON.
    // Note: we do NOT need Box<Document> here because Document itself
    // internally stores a Vec, and Vec already heap-allocates. If we
    // stored Value directly inside itself without indirection, the type
    // would have infinite size — Rust would refuse to compile it.
    Document(Document),
}

/// A document is an ordered collection of key-value pairs.
///
/// We deliberately use `Vec<(String, Value)>` instead of `HashMap<String, Value>`.
/// Why? Two reasons:
///   1. Real BSON documents preserve insertion order — HashMap does not.
///   2. Documents are usually small (a handful of fields), so a linear scan
///      over a Vec is actually faster than hashing in practice.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Document {
    fields: Vec<(String, Value)>,
}

impl Document {
    pub fn new() -> Self {
        Document { fields: Vec::new() }
    }

    /// Insert or update a field. Returns the previous value if the key
    /// already existed (this mirrors the convention set by HashMap::insert).
    ///
    /// `impl Into<String>` and `impl Into<Value>` are "generic over
    /// conversion": it lets callers pass a &str, a String, an i64, a bool,
    /// etc. and have it automatically converted, so you can write
    /// `doc.insert("age", 30)` instead of `doc.insert("age".to_string(), Value::Int(30))`.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        let key = key.into();
        let value = value.into();

        if let Some(existing) = self.fields.iter_mut().find(|(k, _)| k == &key) {
            // std::mem::replace swaps in a new value and hands back the old
            // one — this is how you move a value out of a struct field
            // you only have a mutable *reference* to, without violating
            // Rust's ownership rules.
            Some(std::mem::replace(&mut existing.1, value))
        } else {
            self.fields.push((key, value));
            None
        }
    }

    /// Look up a field by key. Returns `None` if it doesn't exist — Rust
    /// has no `null`; `Option<T>` is how absence is represented, and the
    /// compiler forces callers to handle the "not found" case.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let pos = self.fields.iter().position(|(k, _)| k == key)?;
        Some(self.fields.remove(pos).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = &(String, Value)> {
        self.fields.iter()
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

// `From` impls give us ergonomic construction: `Value::from(30)` or,
// thanks to our `impl Into<Value>` bound above, just `doc.insert("age", 30)`.
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Int(n)
    }
}
impl From<i32> for Value {
    fn from(n: i32) -> Self {
        Value::Int(n as i64)
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::Array(v)
    }
}
impl From<Document> for Value {
    fn from(d: Document) -> Self {
        Value::Document(d)
    }
}

// Unit tests live right next to the code they test — this is Rust
// convention, not a separate tests/ folder (that's reserved for
// integration tests that exercise your crate as a black box).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get() {
        let mut doc = Document::new();
        doc.insert("name", "Alice");
        doc.insert("age", 30);
        doc.insert("active", true);

        assert_eq!(doc.get("name"), Some(&Value::Str("Alice".to_string())));
        assert_eq!(doc.get("age"), Some(&Value::Int(30)));
        assert_eq!(doc.get("missing"), None);
    }

    #[test]
    fn insert_overwrites_and_returns_old_value() {
        let mut doc = Document::new();
        doc.insert("age", 30);
        let old = doc.insert("age", 31);

        assert_eq!(old, Some(Value::Int(30)));
        assert_eq!(doc.get("age"), Some(&Value::Int(31)));
        assert_eq!(doc.len(), 1); // still one field, not two
    }

    #[test]
    fn nested_documents() {
        let mut address = Document::new();
        address.insert("city", "Bengaluru");

        let mut person = Document::new();
        person.insert("name", "Alice");
        person.insert("address", address);

        match person.get("address") {
            Some(Value::Document(inner)) => {
                assert_eq!(inner.get("city"), Some(&Value::Str("Bengaluru".to_string())));
            }
            _ => panic!("expected a nested document"),
        }
    }
}
