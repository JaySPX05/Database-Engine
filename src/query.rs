//! A small MongoDB-style query engine.
//!
//! A query is itself just a `Document` — this module's whole job is
//! `matches(doc, query) -> bool`, deciding whether a document satisfies
//! a query document. Top-level fields in a query are implicitly ANDed,
//! matching Mongo's convention (`{"a": 1, "b": 2}` means a=1 AND b=2).
//!
//! Supported shapes:
//!   - `{"field": value}`              -> equality
//!   - `{"field": {"$gt": value}}`     -> comparison operators
//!   - `{"$and": [query, query, ...]}` -> all sub-queries must match
//!   - `{"$or":  [query, query, ...]}` -> at least one sub-query matches
//!
//! Operators: $eq, $ne, $gt, $gte, $lt, $lte, $in, $nin, $exists.

use crate::document::{Document, Value};
use std::cmp::Ordering;

/// Does `doc` satisfy `query`?
pub fn matches(doc: &Document, query: &Document) -> bool {
    query.iter().all(|(key, condition)| match key.as_str() {
        "$and" => as_subqueries(condition).iter().all(|q| matches(doc, q)),
        "$or" => as_subqueries(condition).iter().any(|q| matches(doc, q)),
        _ => matches_field(doc.get(key), condition),
    })
}

fn as_subqueries(condition: &Value) -> Vec<&Document> {
    match condition {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| match v {
                Value::Document(d) => Some(d),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn matches_field(field_value: Option<&Value>, condition: &Value) -> bool {
    match condition {
        // A nested document whose keys are operators (start with '$')
        // means "apply every operator, all must pass." A nested document
        // WITHOUT operator keys is a literal value to compare for equality
        // (so `{"address": {"city": "..."}}` still works as a plain
        // equality match against a nested document field).
        Value::Document(ops) if is_operator_doc(ops) => {
            ops.iter().all(|(op, operand)| apply_operator(field_value, op, operand))
        }
        other => field_value == Some(other),
    }
}

fn is_operator_doc(doc: &Document) -> bool {
    doc.iter().next().map(|(k, _)| k.starts_with('$')).unwrap_or(false)
}

fn apply_operator(field_value: Option<&Value>, op: &str, operand: &Value) -> bool {
    match op {
        "$eq" => field_value == Some(operand),
        "$ne" => field_value != Some(operand),
        "$exists" => {
            let should_exist = matches!(operand, Value::Bool(true));
            field_value.is_some() == should_exist
        }
        "$in" => match operand {
            Value::Array(items) => field_value.map(|v| items.contains(v)).unwrap_or(false),
            _ => false,
        },
        "$nin" => match operand {
            Value::Array(items) => !field_value.map(|v| items.contains(v)).unwrap_or(false),
            _ => true,
        },
        "$gt" | "$gte" | "$lt" | "$lte" => {
            let Some(fv) = field_value else { return false };
            let Some(ordering) = compare(fv, operand) else { return false };
            match op {
                "$gt" => ordering == Ordering::Greater,
                "$gte" => ordering != Ordering::Less,
                "$lt" => ordering == Ordering::Less,
                "$lte" => ordering != Ordering::Greater,
                _ => unreachable!(),
            }
        }
        // Unknown operator: fail closed rather than silently matching
        // everything — a typo'd operator should never look like "no filter."
        _ => false,
    }
}

/// Compare two values if they're a comparable pair. Int/Float compare
/// numerically across types (so `{"age": {"$gt": 25}}` matches an age
/// stored as either an Int or a Float); mismatched types (e.g. comparing
/// a Str to an Int) are simply not comparable.
fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.partial_cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)),
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

// --- Ergonomic query-building helpers ---------------------------------
// These just build the same Document/Value shapes matches() understands,
// so `query::gt(25)` is shorthand for manually constructing
// `Document::new().insert("$gt", 25)` wrapped in a Value::Document.

pub fn gt(value: impl Into<Value>) -> Value {
    operator("$gt", value)
}
pub fn gte(value: impl Into<Value>) -> Value {
    operator("$gte", value)
}
pub fn lt(value: impl Into<Value>) -> Value {
    operator("$lt", value)
}
pub fn lte(value: impl Into<Value>) -> Value {
    operator("$lte", value)
}
pub fn ne(value: impl Into<Value>) -> Value {
    operator("$ne", value)
}
pub fn exists(should_exist: bool) -> Value {
    operator("$exists", should_exist)
}
pub fn in_(values: Vec<Value>) -> Value {
    operator("$in", Value::Array(values))
}

fn operator(name: &str, value: impl Into<Value>) -> Value {
    let mut doc = Document::new();
    doc.insert(name, value.into());
    Value::Document(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(name: &str, age: i64) -> Document {
        let mut doc = Document::new();
        doc.insert("name", name);
        doc.insert("age", age);
        doc
    }

    #[test]
    fn plain_equality() {
        let doc = person("Alice", 30);
        let mut query = Document::new();
        query.insert("name", "Alice");
        assert!(matches(&doc, &query));

        let mut query2 = Document::new();
        query2.insert("name", "Bob");
        assert!(!matches(&doc, &query2));
    }

    #[test]
    fn comparison_operators() {
        let doc = person("Alice", 30);

        let mut gt_query = Document::new();
        gt_query.insert("age", gt(25));
        assert!(matches(&doc, &gt_query));

        let mut lt_query = Document::new();
        lt_query.insert("age", lt(25));
        assert!(!matches(&doc, &lt_query));

        let mut gte_query = Document::new();
        gte_query.insert("age", gte(30));
        assert!(matches(&doc, &gte_query));

        let mut lte_query = Document::new();
        lte_query.insert("age", lte(29));
        assert!(!matches(&doc, &lte_query));
    }

    #[test]
    fn implicit_and_across_fields() {
        let doc = person("Alice", 30);

        let mut query = Document::new();
        query.insert("name", "Alice");
        query.insert("age", gt(25));
        assert!(matches(&doc, &query));

        let mut failing_query = Document::new();
        failing_query.insert("name", "Alice");
        failing_query.insert("age", gt(100));
        assert!(!matches(&doc, &failing_query));
    }

    #[test]
    fn in_and_nin() {
        let doc = person("Alice", 30);

        let mut in_query = Document::new();
        in_query.insert("name", in_(vec![Value::from("Alice"), Value::from("Bob")]));
        assert!(matches(&doc, &in_query));

        let mut nin_query = Document::new();
        nin_query.insert("name", Value::Document({
            let mut d = Document::new();
            d.insert("$nin", Value::Array(vec![Value::from("Bob"), Value::from("Carol")]));
            d
        }));
        assert!(matches(&doc, &nin_query));
    }

    #[test]
    fn exists_operator() {
        let doc = person("Alice", 30);

        let mut has_age = Document::new();
        has_age.insert("age", exists(true));
        assert!(matches(&doc, &has_age));

        let mut has_email = Document::new();
        has_email.insert("email", exists(true));
        assert!(!matches(&doc, &has_email));

        let mut no_email = Document::new();
        no_email.insert("email", exists(false));
        assert!(matches(&doc, &no_email));
    }

    #[test]
    fn or_combinator() {
        let doc = person("Alice", 30);

        let mut young_query = Document::new();
        young_query.insert("age", lt(20));

        let mut alice_query = Document::new();
        alice_query.insert("name", "Alice");

        let mut or_query = Document::new();
        or_query.insert(
            "$or",
            Value::Array(vec![Value::Document(young_query), Value::Document(alice_query)]),
        );
        assert!(matches(&doc, &or_query));
    }

    #[test]
    fn and_combinator() {
        let doc = person("Alice", 30);

        let mut adult_query = Document::new();
        adult_query.insert("age", gte(18));

        let mut named_alice = Document::new();
        named_alice.insert("name", "Alice");

        let mut and_query = Document::new();
        and_query.insert(
            "$and",
            Value::Array(vec![Value::Document(adult_query), Value::Document(named_alice)]),
        );
        assert!(matches(&doc, &and_query));
    }

    #[test]
    fn cross_type_numeric_comparison() {
        let mut doc = Document::new();
        doc.insert("score", 90.5);

        let mut query = Document::new();
        query.insert("score", gt(90)); // Int compared against a Float field
        assert!(matches(&doc, &query));
    }
}
