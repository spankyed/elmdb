//! What a read answers (`docs/architecture/engine.md`, Reads: plans), as types: the definition of the answers'
//! protocol, generated as TypeScript into the binding's `protocol/` (`Answers.ts`, `Answer.ts` and the types they name).
//!
//! `reads.rs` writes an answer's msgpack itself, splicing each field's stored bytes in unchanged, so no value is decoded
//! to be answered; these types are what those bytes decode to. `conforms` holds the bytes to them, field for field: the
//! engine's own tests run it on every read they make (`reads::run`), and `tests/answers.rs` reads each kind of answer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A field's value, its stored msgpack as the caller encoded it
pub type Value = rmpv::Value;

/// What a call's reads answer: an answer per read, in the order asked
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct Answers(pub Vec<Answer>);

/// One read's answer. A plan's terminal names its kind, and so does each direct read; `visits` and `explain` wrap a
/// plan's answer in `visited` and `explained`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Answer {
    /// `ids`: the set's ids, in its order
    Ids { ids: Vec<String> },
    /// `count`: how many entities the set holds
    Count { n: u64 },
    /// `exists`, and `wouldCreateCycle`
    Bool { value: bool },
    /// `first` and `last`: the entity's id, or `null` for an empty set
    Id { id: Option<String> },
    /// `rows`: each entity's row
    Rows {
        rows: Vec<Row>,
        /// No map in `rows` may hold a key `__proto__`, so the caller may decode them straight to objects
        plain_objects: bool,
    },
    /// `firstRow`, and a `row` read: the entity's row, or `null`
    Row {
        row: Option<Row>,
        /// No map in `row` may hold a key `__proto__`
        plain_objects: bool,
    },
    /// A `field` read: whether the entity holds the field, and its value, `null` when it doesn't
    Field {
        present: bool,
        #[cfg_attr(feature = "codegen", ts(type = "unknown"))]
        value: Value,
    },
    /// A `relation` read: the relation's row, or `null`
    Relation { relation: Option<RelationRow> },
    /// `edges`, and `findRelations`: the relations' rows
    Relations { relations: Vec<RelationRow> },
    /// `degrees`: each entity of the set with how many edges of the kinds it has
    Degrees { degrees: Vec<(String, u64)> },
    /// `weakComponents`: the groups, largest first
    WeakComponents { weak_components: Components },
    /// `strongComponents`: the groups, largest first
    StrongComponents { strong_components: Components },
    /// `communities`: the groups' ids, largest first
    Communities { communities: Vec<Vec<String>> },
    /// `least`: the entity whose field is least, and that value; both `null` when none holds one
    Least {
        id: Option<String>,
        #[cfg_attr(feature = "codegen", ts(type = "unknown"))]
        value: Value,
    },
    /// `greatest`: the entity whose field is greatest, and that value; both `null` when none holds one
    Greatest {
        id: Option<String>,
        #[cfg_attr(feature = "codegen", ts(type = "unknown"))]
        value: Value,
    },
    /// `sum`: the numbers the field holds over the set, summed, and how many entities hold one
    Sum { sum: f64, n: u64 },
    /// `groupBy`: each value with the ids holding it, or with `counts` how many, in the order each value was first met
    Groups {
        #[cfg_attr(feature = "codegen", ts(type = "Array<[unknown, Array<string>]> | Array<[unknown, number]>"))]
        groups: Groups,
        /// No map in `groups` may hold a key `__proto__`
        plain_objects: bool,
    },
    /// `hits`: what a search, a rank or a `similar` found in each entity of the set
    Hits {
        /// A `rank`'s rounds, or a `ranked` index's bound; absent for a plan with neither
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "codegen", ts(optional))]
        rank: Option<HitsRank>,
        hits: Vec<Hit>,
        /// No map in `hits` may hold a key `__proto__`
        plain_objects: bool,
    },
    /// A `findLabels` read: the labels matching its query, best first
    Labels { hits: Vec<LabelHit> },
    /// A `suggest` read: the index's terms that complete the term, and those that correct it
    Suggestions { completions: Vec<String>, corrections: Vec<String> },
    /// A `pendingEmbeddings` read: the vector index, its model and dimensions, how many chunks wait for an embedding,
    /// and the first of them
    Pending { index: String, model: String, dims: u32, count: u64, chunks: Vec<PendingChunk> },
    /// A `catalog` read
    Catalog { types: Vec<CatalogType>, relations: Vec<CatalogKind>, roles: Vec<String> },
    /// A query asked for its `visits`: its answer, and the entities its start and walks read
    Visited { answer: Box<Answer>, visited: u64 },
    /// A query asked to `explain`: its answer, its visits, the milliseconds the read took, and each stage
    Explained { answer: Box<Answer>, visited: u64, ms: f64, stages: Vec<Stage> },
}

/// An entity's row: its id and the fields it holds, each value as stored. A whole row holds `entityType` and every
/// field; one of the fields asked for holds those of them the entity holds
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[cfg_attr(feature = "codegen", ts(type = "{ id: string, entityType?: string, [field: string]: unknown }"))]
pub struct Row {
    pub id: String,
    /// Every key but `id`, `entityType` among them where the row holds it
    pub fields: BTreeMap<String, Value>,
}

impl Serialize for Row {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.fields.len() + 1))?;
        map.serialize_entry("id", &self.id)?;
        for (name, value) in &self.fields {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Row {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let mut fields = BTreeMap::<String, Value>::deserialize(deserializer)?;
        let id = match fields.remove("id") {
            Some(Value::String(id)) => id.into_str().ok_or_else(|| D::Error::custom("a row's id isn't UTF-8"))?,
            Some(other) => return Err(D::Error::custom(format!("a row's id is {other}, not a string"))),
            None => return Err(D::Error::custom("a row has no id")),
        };
        if let Some(entity_type) = fields.get("entityType")
            && !entity_type.is_str()
        {
            return Err(D::Error::custom(format!("a row's entityType is {entity_type}, not a string")));
        }
        Ok(Row { id, fields })
    }
}

/// A relation's row
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RelationRow {
    pub id: String,
    pub kind: String,
    pub source: String,
    pub target: String,
    /// What the link carried, or `null`
    #[cfg_attr(feature = "codegen", ts(type = "unknown"))]
    pub info: Value,
    /// The relation's weight, for a kind declared weighted: the one it was linked with, or 1
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub weight: Option<f64>,
}

/// A component answer's groups: each group's ids in the set's order, or with `counts` each group's size
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(untagged)]
pub enum Components {
    Members(Vec<Vec<String>>),
    Sizes(Vec<u32>),
}

/// A `groupBy`'s groups: each value with the ids holding it, or with `counts` how many hold it
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Groups {
    Members(Vec<(Value, Vec<String>)>),
    Counts(Vec<(Value, u64)>),
}

/// How a `hits` answer's scores were ranked: a `rank`'s rounds, or the bound on a `ranked` index's scores
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(untagged)]
pub enum HitsRank {
    /// Whether a `rank`'s scores settled, and in how many rounds
    Rank { converged: bool, iterations: u32 },
    /// A `ranked` read's index, and the bound on its scores' L1 distance from PageRank
    Ranked { index: String, error: f64 },
}

/// What a search, a rank or a `similar` found in one entity
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub id: String,
    pub score: f64,
    /// The query's terms it matched
    pub terms: Vec<String>,
    /// With `highlights`: where its terms are in each field, `[element, start, end]` in UTF-16 units
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub highlights: Option<BTreeMap<String, Vec<[u32; 3]>>>,
    /// With `fields`: its id and those of them it holds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub row: Option<Row>,
}

/// A label matching a `findLabels` query
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct LabelHit {
    pub id: String,
    pub score: f64,
    /// The label's characters the query matched
    pub positions: Vec<u32>,
}

/// A chunk waiting for its embedding
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct PendingChunk {
    pub id: String,
    pub chunk: u32,
    pub hash: String,
    pub text: String,
}

/// A declared type in a `catalog`: its live entities, and the fields its entities have held
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CatalogType {
    pub name: String,
    pub count: u64,
    pub fields: Vec<String>,
}

/// A declared relation kind in a `catalog`, with its relations
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CatalogKind {
    pub kind: String,
    pub count: u64,
}

/// One stage of an explained plan: its start, a step or its terminal (`docs/architecture/engine.md`, EXPLAIN)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Stage {
    /// The start's, the step's or the terminal's name: `types`, `where`, `orderBy`, `rows`, …
    pub stage: String,
    /// The index the stage read, each named where it read several as lists, or `none: walks the type`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub index: Option<String>,
    /// A stage that built a graph: its nodes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub nodes: Option<u64>,
    /// A stage that built a graph: its edges; a `similar`: the edges it read
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub edges: Option<u64>,
    /// A graph's edges' weights added up, where the stage read weights
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub weight: Option<f64>,
    /// A `rank`'s rounds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub iterations: Option<u32>,
    /// Whether a `rank`'s scores settled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub converged: Option<bool>,
    /// A `topological` order's cycles
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub cycles: Option<u32>,
    /// A `ranked` stage's bound on its scores
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub error: Option<f64>,
    /// A `ranked` stage that computed its scores, its index not built yet: always `true` where present
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub building: Option<bool>,
    /// An `orderBy` or a `range` that sorted or walked for want of an index: the fields of the index that would have
    /// read it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub wants: Option<String>,
    /// An `orderBy` that sorted strings though a built index reads their order: why that order wasn't read
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub sorted: Option<Sorted>,
    /// A `search`: how many of its matches it held to rank them
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub held: Option<u64>,
    /// `and` where the stage read the places several lists hold in common
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub set: Option<SetOf>,
    /// How many places of lists or of rows the stage was handed, each one walked or sought
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub places: Option<u64>,
    /// A `search` that ran within its filters' places: how many
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub within: Option<u64>,
    /// A `similar`: the neighbours of `to`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub neighbours: Option<u64>,
    /// A `similar`: the candidates it scored
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub candidates: Option<u64>,
    /// What the stage before handed it, and what it read from the store itself
    pub visited: u64,
    /// What it handed on; a terminal's is what it answers
    pub kept: u64,
    /// The milliseconds spent in the stage's own code
    pub ms: f64,
}

/// Why an `orderBy` sorted strings a built index reads the order of
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Sorted {
    /// The index's names and the equalities' values leave its order's keys no room
    NoRoom,
    /// Its order isn't yet one this elmdb filed whole
    Building,
}

/// How a stage combined the lists of places it read
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum SetOf {
    /// The places every list holds
    And,
}

impl Answers {
    /// The answers in a read's msgpack bytes
    pub fn decode(bytes: &[u8]) -> Result<Answers, String> {
        rmp_serde::from_slice(bytes).map_err(|e| e.to_string())
    }
}

/// Whether a read's msgpack bytes are exactly what `Answers` declares: they decode into it, and encoding what they
/// decoded to gives the same msgpack values back, every map holding the same keys, so a key the bytes hold that no
/// type declares, or one a type declares that the bytes lack, is told
pub fn conforms(bytes: &[u8]) -> Result<Answers, String> {
    let answers = Answers::decode(bytes)?;
    let held = rmpv::decode::read_value(&mut &bytes[..]).map_err(|e| e.to_string())?;
    let again = rmp_serde::to_vec_named(&answers).map_err(|e| e.to_string())?;
    let declared = rmpv::decode::read_value(&mut &again[..]).map_err(|e| e.to_string())?;
    let (held, declared) = (normal(held), normal(declared));
    if held != declared {
        return Err(format!("the answer's bytes hold {held}, and what they decode to is {declared}"));
    }
    Ok(answers)
}

/// A value with each map's entries in the order of their keys, so two maps holding the same entries compare equal
/// whatever order they were written in
fn normal(value: Value) -> Value {
    match value {
        Value::Map(entries) => {
            let mut entries: Vec<(Value, Value)> = entries.into_iter().map(|(k, v)| (normal(k), normal(v))).collect();
            entries.sort_by_key(|(key, _)| key.to_string());
            Value::Map(entries)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(normal).collect()),
        other => other,
    }
}
