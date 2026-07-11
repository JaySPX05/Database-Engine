//! Binary serialization for `Value` / `Document`.
//!
//! This is a small, hand-rolled binary format (similar in spirit to BSON):
//! every value is a 1-byte tag followed by variant-specific payload bytes.
//! See the table in the accompanying explanation for the exact layout.

use crate::document::{Document, Value};

/// All the ways decoding can fail. Having a dedicated error enum (rather
/// than e.g. panicking, or returning a bare String) means callers can
/// pattern-match on *why* it failed and the compiler tracks every failure
/// path for us.
#[derive(Debug, PartialEq)]
pub enum DecodeError {
    /// We ran off the end of the byte slice before finishing.
    UnexpectedEof,
    /// A tag byte didn't match any known Value variant.
    InvalidTag(u8),
    /// A string's bytes weren't valid UTF-8.
    InvalidUtf8,
}

// Implementing `std::fmt::Display` lets us do `println!("{err}")`.
impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::UnexpectedEof => write!(f, "unexpected end of input"),
            DecodeError::InvalidTag(t) => write!(f, "invalid value tag: {t}"),
            DecodeError::InvalidUtf8 => write!(f, "invalid utf-8 in string"),
        }
    }
}

// Implementing `std::error::Error` (a marker/interop trait, no required
// methods here) lets our error type plug into the rest of Rust's error
// ecosystem later — e.g. the `?` operator can convert it into a boxed
// "any error" type when we build the storage layer.
impl std::error::Error for DecodeError {}

/// A `Cursor` walks forward through a byte slice, tracking how far we've
/// read. Every `read_*` method either advances the cursor and returns the
/// value, or returns `Err` without corrupting `self.pos` further than
/// necessary. `<'a>` is a *lifetime*: it tells the compiler "this Cursor
/// can't outlive the byte slice it's borrowing from."
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Cursor { bytes, pos: 0 }
    }

    fn read_u8(&mut self) -> Result<u8, DecodeError> {
        let byte = *self.bytes.get(self.pos).ok_or(DecodeError::UnexpectedEof)?;
        self.pos += 1;
        Ok(byte)
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::UnexpectedEof)?;
        let slice = self.bytes.get(self.pos..end).ok_or(DecodeError::UnexpectedEof)?;
        self.pos = end;
        Ok(slice)
    }

    fn read_u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.read_bytes(4)?;
        // try_into() converts the &[u8] slice into a fixed-size [u8; 4]
        // array; it can theoretically fail if the slice is the wrong
        // length, but we just asked for exactly 4 bytes, so .unwrap() here
        // is safe (this is a case where "unwrap is fine" because we've
        // already proven it can't fail).
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_i64(&mut self) -> Result<i64, DecodeError> {
        let bytes = self.read_bytes(8)?;
        Ok(i64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_f64(&mut self) -> Result<f64, DecodeError> {
        let bytes = self.read_bytes(8)?;
        Ok(f64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_string(&mut self) -> Result<String, DecodeError> {
        let len = self.read_u32()? as usize;
        let bytes = self.read_bytes(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
    }
}

// --- Encoding: Value/Document -> bytes -------------------------------

impl Value {
    /// Append this value's binary representation to `buf`.
    ///
    /// We take `&mut Vec<u8>` rather than returning a new Vec so that
    /// encoding a whole Document doesn't allocate a fresh buffer for every
    /// nested field — everything gets appended to one growing buffer.
    pub fn encode(&self, buf: &mut Vec<u8>) {
        match self {
            Value::Null => buf.push(0),
            Value::Bool(b) => {
                buf.push(1);
                buf.push(if *b { 1 } else { 0 });
            }
            Value::Int(n) => {
                buf.push(2);
                buf.extend_from_slice(&n.to_le_bytes());
            }
            Value::Float(f) => {
                buf.push(3);
                buf.extend_from_slice(&f.to_le_bytes());
            }
            Value::Str(s) => {
                buf.push(4);
                encode_string(s, buf);
            }
            Value::Array(items) => {
                buf.push(5);
                buf.extend_from_slice(&(items.len() as u32).to_le_bytes());
                for item in items {
                    item.encode(buf);
                }
            }
            Value::Document(doc) => {
                buf.push(6);
                doc.encode(buf);
            }
        }
    }

    fn decode(cursor: &mut Cursor) -> Result<Value, DecodeError> {
        let tag = cursor.read_u8()?;
        match tag {
            0 => Ok(Value::Null),
            1 => Ok(Value::Bool(cursor.read_u8()? != 0)),
            2 => Ok(Value::Int(cursor.read_i64()?)),
            3 => Ok(Value::Float(cursor.read_f64()?)),
            4 => Ok(Value::Str(cursor.read_string()?)),
            5 => {
                let count = cursor.read_u32()? as usize;
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(Value::decode(cursor)?);
                }
                Ok(Value::Array(items))
            }
            6 => Ok(Value::Document(Document::decode(cursor)?)),
            other => Err(DecodeError::InvalidTag(other)),
        }
    }
}

impl Document {
    /// Encode this document to a fresh byte buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.encode(&mut buf);
        buf
    }

    /// Decode a document from a byte slice. This is the public entry
    /// point; it wraps the byte slice in a Cursor and delegates to the
    /// internal recursive decoder.
    pub fn from_bytes(bytes: &[u8]) -> Result<Document, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        Document::decode(&mut cursor)
    }

    fn encode(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.len() as u32).to_le_bytes());
        for (key, value) in self.iter() {
            encode_string(key, buf);
            value.encode(buf);
        }
    }

    fn decode(cursor: &mut Cursor) -> Result<Document, DecodeError> {
        let count = cursor.read_u32()? as usize;
        let mut doc = Document::new();
        for _ in 0..count {
            let key = cursor.read_string()?;
            let value = Value::decode(cursor)?;
            doc.insert(key, value);
        }
        Ok(doc)
    }
}

fn encode_string(s: &str, buf: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_scalars() {
        for value in [
            Value::Null,
            Value::Bool(true),
            Value::Int(-42),
            Value::Float(3.14159),
            Value::Str("hello, docdb".to_string()),
        ] {
            let mut buf = Vec::new();
            value.encode(&mut buf);

            let mut cursor = Cursor::new(&buf);
            let decoded = Value::decode(&mut cursor).unwrap();
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn round_trip_document() {
        let mut inner = Document::new();
        inner.insert("city", "Bengaluru");
        inner.insert("pincode", 560001);

        let mut doc = Document::new();
        doc.insert("name", "Alice");
        doc.insert("age", 30);
        doc.insert("scores", Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]));
        doc.insert("address", inner);

        let bytes = doc.to_bytes();
        let decoded = Document::from_bytes(&bytes).unwrap();

        assert_eq!(decoded, doc);
    }

    #[test]
    fn truncated_bytes_produce_error_not_panic() {
        let mut doc = Document::new();
        doc.insert("name", "Alice");
        let bytes = doc.to_bytes();

        // Chop the buffer off mid-way through — decoding should fail
        // gracefully with an error, never panic or read out of bounds.
        let truncated = &bytes[..bytes.len() - 2];
        let result = Document::from_bytes(truncated);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_tag_is_rejected() {
        // field count = 1 (u32 LE) | key length = 0 (u32 LE) | tag = 99 (invalid)
        let bytes = [1u8, 0, 0, 0, 0, 0, 0, 0, 99];
        let result = Document::from_bytes(&bytes);
        assert_eq!(result, Err(DecodeError::InvalidTag(99)));
    }
}
