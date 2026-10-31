//! The write protocol (`docs/architecture/engine.md`, Writes): a batch of operations applied in order in one unit, and what it did.
//! A batch crosses the binding as one msgpack buffer; a field's value is msgpack inside it, held as `rmpv::Value`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A field's value as a batch carries it
pub type Value = rmpv::Value;

/// Writes applied in order, in the unit open or in one of their own
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Batch {
    pub ops: Vec<Op>,
    /// The time the batch's writes are stamped with, in milliseconds since the epoch; the engine's clock by default
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub now: Option<f64>,
}

/// One write. Every write to an entity names it by id, and refuses an id no entity has (`NO_SUCH_ENTITY`)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Op {
    /// A new entity: its whole row, which holds every field its type requires. The engine assigns the id, the short
    /// code and the times unless an import brings them
    Create {
        #[serde(rename = "type")]
        entity_type: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        id: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(type = "Record<string, unknown> | undefined", optional))]
        fields: BTreeMap<String, Value>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        short_code: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        created_at: Option<f64>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        updated_at: Option<f64>,
    },
    /// Writes the fields to the entity with the id, creating it as `type` with them when no entity has it
    Upsert {
        #[serde(rename = "type")]
        entity_type: String,
        id: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(type = "Record<string, unknown> | undefined", optional))]
        fields: BTreeMap<String, Value>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        short_code: Option<String>,
    },
    /// Sets each field. `touch` stamps `updatedAt`, as every attribute write does unless told not to
    Set {
        id: String,
        #[cfg_attr(feature = "codegen", ts(type = "Record<string, unknown>"))]
        fields: BTreeMap<String, Value>,
        #[serde(default = "yes")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        touch: bool,
    },
    /// Appends to the field's list, starting one when it has none
    Append {
        id: String,
        field: String,
        #[cfg_attr(feature = "codegen", ts(type = "unknown"))]
        value: Value,
        #[serde(default = "yes")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        touch: bool,
    },
    /// Merges into the field's map, or sets it when either isn't a map
    Merge {
        id: String,
        field: String,
        #[cfg_attr(feature = "codegen", ts(type = "unknown"))]
        value: Value,
        #[serde(default = "yes")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        touch: bool,
    },
    /// Removes the field
    Delete {
        id: String,
        field: String,
        #[serde(default = "yes")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        touch: bool,
    },
    /// Removes the list's element at `index`; an index past its end does nothing
    RemoveAt {
        id: String,
        field: String,
        index: u32,
        #[serde(default = "yes")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        touch: bool,
    },
    /// Removes the list's first element equal to `value`, or, for a map, the first holding each of its fields
    RemoveValue {
        id: String,
        field: String,
        #[cfg_attr(feature = "codegen", ts(type = "unknown"))]
        value: Value,
        #[serde(default = "yes")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        touch: bool,
    },
    /// Puts the entity in the trash, at `at` or now; a trashed entity keeps the time it was first trashed
    Trash {
        id: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        at: Option<f64>,
    },
    Restore {
        id: String,
    },
    Grant {
        id: String,
        role: String,
    },
    Revoke {
        id: String,
        role: String,
    },
    /// Grants the role, revoking it from every other entity of `among` that holds it
    GrantExclusive {
        id: String,
        role: String,
        among: Vec<String>,
    },
    /// An edge of `kind` from `source` to `target`. Idempotent on the whole edge: linking it again keeps its id, and no
    /// `info` (or no `weight`) matches any edge between the ends. The target must exist when the unit commits
    Link {
        source: String,
        kind: String,
        target: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(type = "unknown", optional))]
        info: Option<Value>,
        /// The edge's weight, for a kind declared `weighted`: a number from 0 to 2^960, 1 when none is given
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        weight: Option<f64>,
    },
    /// Removes the source's edges of `kind` to `target`; with no kind, of every kind; with no target, to every target
    Unlink {
        source: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        kind: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        target: Option<String>,
    },
    /// Moves a relation's ends or replaces its info or its weight, keeping its id
    UpdateRelation {
        relation: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        source: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        target: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(type = "unknown", optional))]
        info: Option<Value>,
        /// The edge's weight, for a kind declared `weighted`: a number from 0 to 2^960, 1 when none is given
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        weight: Option<f64>,
    },
    RemoveRelation {
        relation: String,
    },
    /// Removes the entity, its fields and every relation to or from it
    Destroy {
        id: String,
    },
    /// The next `<base> <n>` no entity of the type holds as its `label`, numbered per type and base
    TakeLabel {
        #[serde(rename = "type")]
        entity_type: String,
        base: String,
    },
    /// A caller's embedding of one chunk of an entity's text in a vector index (`pendingEmbeddings` lists them):
    /// stored when `hash` is still the chunk's, and answered `stale` when the text has changed since
    Embed {
        id: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        index: Option<String>,
        chunk: u32,
        hash: String,
        model: String,
        vector: Vec<f32>,
    },
}

fn yes() -> bool {
    true
}

/// What each operation answered, in the batch's order, and what the batch changed
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    pub results: Vec<OpResult>,
}

/// One operation's answer
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum OpResult {
    /// The operation wrote and has nothing to say
    Done,
    /// An entity was written: its id, its short code if it has one, and whether this operation created it
    Entity {
        id: String,
        short_code: Option<String>,
        created: bool,
    },
    /// A relation was linked or kept: its id, and whether this operation linked it
    Relation {
        id: String,
        linked: bool,
    },
    Label {
        label: String,
    },
    /// An embedding not stored: its chunk's text changed after it was read
    Stale,
}
