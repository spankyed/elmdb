//! The schema: what a database holds and the rules it keeps, declared as data (`docs/architecture/schema.md`).

mod diff;
mod model;
mod parse;
mod validate;

pub use diff::{Change, Effect, SchemaChange, diff};
pub use model::*;
pub use parse::{Problem, parse};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{Error, ErrorCode};

impl Schema {
    /// Reads and validates a schema, refusing it with every problem listed (`SCHEMA_INVALID`)
    pub fn from_json(json: &Value) -> crate::Result<Self> {
        parse(json).map(Self::normalized).map_err(|problems| {
            let lines: Vec<String> = problems.iter().map(ToString::to_string).collect();
            Error::new(
                ErrorCode::SchemaInvalid,
                format!(
                    "The schema has {} problem{}:\n{}",
                    lines.len(),
                    if lines.len() == 1 { "" } else { "s" },
                    lines.join("\n")
                ),
            )
        })
    }

    /// The canonical form: every key written, maps' keys sorted, the lists whose order means nothing (a relation's
    /// endpoints, a `ref`'s types, an `enum`'s values) sorted too, no whitespace. Two schemas that mean the same thing
    /// have the same canonical form
    pub fn canonical(&self) -> String {
        let value = serde_json::to_value(self.clone().normalized()).expect("a schema always serializes");
        sorted(value).to_string()
    }

    /// The schema with its lists whose order means nothing sorted, as reading one gives it
    pub fn normalized(mut self) -> Self {
        for relation in self.relations.values_mut() {
            if let Some(endpoints) = &mut relation.endpoints {
                endpoints.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));
                endpoints.dedup_by(|a, b| a.source == b.source && a.target == b.target);
            }
        }
        for def in self.types.values_mut() {
            for field in def.fields.values_mut() {
                sort_sets(&mut field.value);
            }
        }
        self
    }

    /// The SHA-256 of the canonical form, in hex
    pub fn hash(&self) -> String {
        let digest = Sha256::digest(self.canonical().as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Reads a schema back from its canonical form, as a database stores it
    pub fn from_canonical(text: &str) -> crate::Result<Self> {
        serde_json::from_str(text)
            .map_err(|e| Error::new(ErrorCode::Corrupted, format!("The stored schema doesn't read: {e}")))
    }
}

/// A value type's lists that are sets, sorted, through a list's element type
fn sort_sets(value: &mut model::ValueType) {
    match value {
        model::ValueType::Enum { values } => {
            values.sort();
            values.dedup();
        }
        model::ValueType::Ref { to } => {
            to.sort();
            to.dedup();
        }
        model::ValueType::List { of } => sort_sets(of),
        _ => {}
    }
}

fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().map(|(k, v)| (k, sorted(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests;
