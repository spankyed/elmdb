//! The read protocol (`docs/engine.md`, Reads): reads sent as data, several in one call, each a query plan or a direct
//! read, answered in one envelope. A caller's own function can't cross, so a caller with one runs the plan up to it and
//! goes on from the ids.

use serde::{Deserialize, Serialize};

/// Reads answered together, from one snapshot
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct Reads {
    pub reads: Vec<ReadOp>,
}

/// One read
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(tag = "read", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ReadOp {
    Query {
        plan: Plan,
    },
    /// The entity's row: its id, type and every field; `null` for an id no entity has. It reads through the trash
    Row {
        id: String,
    },
    /// One field of the entity, the trash read through; `entityType` is read from the id
    Field {
        id: String,
        field: String,
    },
    /// The relation's row, or `null`
    Relation {
        id: String,
    },
    /// The rows of the matching relations: by an end, its edges in the order linked; otherwise every relation
    FindRelations {
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        target: Option<String>,
    },
    /// Whether linking `source` to `target` would close a cycle through these kinds
    WouldCreateCycle {
        source: String,
        target: String,
        kinds: Vec<String>,
    },
}

/// A query: where it starts, the steps after, and what it answers
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    #[serde(default)]
    pub scope: Scope,
    pub start: Start,
    #[serde(default)]
    pub steps: Vec<Step>,
    pub terminal: Terminal,
}

/// Which entities a query reaches
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    /// None in the trash, not as a start and not at a far end
    #[default]
    Live,
    /// The trash too
    WithTrashed,
    /// Starts from the trashed entities only, and its steps reach the trash
    Trashed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(tag = "from", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Start {
    /// The types' entities, in the order they were created; a type the schema doesn't declare is refused
    Types { types: Vec<String> },
    /// These ids, in the order given, each one the scope reaches
    Ids { ids: Vec<String> },
    /// Every entity, in the order they were created
    All,
}

/// One step after the start
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(tag = "step", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Step {
    OfType {
        #[serde(rename = "type")]
        entity_type: String,
    },
    Within {
        ids: Vec<String>,
    },
    /// The entities whose field is `value`, or a list holding it; a `null` matches a field that is null or missing
    Where {
        field: String,
        #[cfg_attr(feature = "codegen", ts(type = "unknown"))]
        value: rmpv::Value,
    },
    Has {
        field: String,
    },
    Lacks {
        field: String,
    },
    WithRole {
        role: String,
    },
    /// The entities with an edge of any kind, either way, to `other`
    LinkedWith {
        other: String,
    },
    /// The entities with an edge of `kind` out to `other`, or with none named, to any entity the query could reach
    LinkedTo {
        kind: String,
        #[serde(default)]
        other: Option<String>,
    },
    LinkedFrom {
        kind: String,
        #[serde(default)]
        other: Option<String>,
    },
    UnlinkedTo {
        kind: String,
        #[serde(default)]
        other: Option<String>,
    },
    UnlinkedFrom {
        kind: String,
        #[serde(default)]
        other: Option<String>,
    },
    /// The far ends of the set's edges of the kinds, each once, of the types when given
    Out {
        kinds: Vec<String>,
        #[serde(default)]
        types: Option<Vec<String>>,
    },
    In {
        kinds: Vec<String>,
        #[serde(default)]
        types: Option<Vec<String>>,
    },
    /// Every entity the set reaches through the kinds, any number of edges deep: nearest first, each once
    Descendants {
        kinds: Vec<String>,
        #[serde(default)]
        types: Option<Vec<String>>,
    },
    Ancestors {
        kinds: Vec<String>,
        #[serde(default)]
        types: Option<Vec<String>>,
    },
    /// By the field's value: booleans, numbers, timestamps, strings (by the schema's collation), anything else by its
    /// JSON, then a missing or null value; ties keep the query's order
    OrderBy {
        field: String,
        #[serde(default)]
        descending: bool,
    },
    Reverse,
    Limit {
        n: f64,
    },
    /// Each entity once, or with a field, the first entity of each value
    Distinct {
        #[serde(default)]
        field: Option<String>,
    },
}

/// What a query answers
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(tag = "answer", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Terminal {
    Ids,
    Count,
    First,
    Last,
    Exists,
    /// Each entity's row, or with fields, its id and those of the fields it holds
    Rows {
        #[serde(default)]
        fields: Option<Vec<String>>,
    },
    /// The first entity's row, or with fields, its id and those of them it holds; `null` when there is none
    FirstRow {
        #[serde(default)]
        fields: Option<Vec<String>>,
    },
    /// Each edge of the kinds out of (or into) the set whose far end the query could reach, as a relation row
    Edges {
        kinds: Vec<String>,
        outward: bool,
        #[serde(default)]
        types: Option<Vec<String>>,
    },
    /// The ids by the field's value, in the order each value first appears
    GroupBy {
        field: String,
    },
}
