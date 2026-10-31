//! The read protocol (`docs/architecture/engine.md`, Reads): reads sent as data, several in one call, each a query plan or a direct
//! read, answered in one envelope. A caller's own function can't cross, so a caller with one runs the plan up to it and
//! goes on from the ids.

use serde::{Deserialize, Serialize};

/// Reads answered together, from one snapshot
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct Reads {
    pub reads: Vec<ReadOp>,
    /// Tracks the reads under this token: what they touch is added to what the token holds, and a commit writing into
    /// it names the token (`touched`) until `untrack` drops it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub track: Option<u32>,
}

/// One read
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(tag = "read", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ReadOp {
    /// A plan's answer; with `visits`, wrapped as `{ kind: "visited", answer, visited }`, `visited` counting the
    /// entities its start and walks reached, which says whether a filter walked rows an index could have answered.
    /// With `explain`, wrapped as `{ kind: "explained", answer, visited, ms, stages }` instead: each stage of the plan —
    /// its start, each step and its terminal — with the index it read, the entities it visited and kept, and the
    /// milliseconds spent in it (`docs/architecture/engine.md`, EXPLAIN)
    Query {
        plan: Plan,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        visits: bool,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        explain: bool,
    },
    /// The entity's row: its id, type and every field; `null` for an id no entity has. It reads through the trash
    Row { id: String },
    /// One field of the entity, the trash read through; `entityType` is read from the id
    Field { id: String, field: String },
    /// The relation's row, or `null`
    Relation { id: String },
    /// The rows of the matching relations: by an end, its edges in the order linked; otherwise every relation
    FindRelations {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        kind: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        source: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        target: Option<String>,
    },
    /// Whether linking `source` to `target` would close a cycle through these kinds
    WouldCreateCycle { source: String, target: String, kinds: Vec<String> },
    /// Terms of a type's text index for a word being typed: those it starts (`completions`, most held first) and the
    /// nearest to it (`corrections`, for "did you mean")
    Suggest {
        #[serde(rename = "type")]
        entity_type: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        index: Option<String>,
        term: String,
        #[serde(default = "ten")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<u32>", optional))]
        limit: u32,
    },
    /// The chunks of a type's vector index waiting for embeddings, up to `limit`, with the model and dimensions to embed
    /// them by and how many wait in all: what a caller's worker embeds and writes back with `embed`
    PendingEmbeddings {
        #[serde(rename = "type")]
        entity_type: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        index: Option<String>,
        #[serde(default = "hundred")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<u32>", optional))]
        limit: u32,
    },
    /// The entities whose label holds the query's characters in order, best first: for a quick-open across types.
    /// `fields` names each type's label field
    FindLabels {
        query: String,
        fields: std::collections::BTreeMap<String, String>,
        #[serde(default = "ten")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<u32>", optional))]
        limit: u32,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Scope>", optional))]
        scope: Scope,
    },
}

fn ten() -> u32 {
    10
}

fn hundred() -> u32 {
    100
}

/// A query: where it starts, the steps after, and what it answers
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<Scope>", optional))]
    pub scope: Scope,
    pub start: Start,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<Step>>", optional))]
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

/// Which of an entity's edges a walk follows: those out of it, those into it, or both
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    #[default]
    Out,
    In,
    Both,
}

impl Direction {
    /// The `outward` of each side of the edges it follows
    pub fn sides(self) -> &'static [bool] {
        match self {
            Direction::Out => &[true],
            Direction::In => &[false],
            Direction::Both => &[true, false],
        }
    }
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
        #[cfg_attr(feature = "codegen", ts(optional))]
        other: Option<String>,
    },
    LinkedFrom {
        kind: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        other: Option<String>,
    },
    UnlinkedTo {
        kind: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        other: Option<String>,
    },
    UnlinkedFrom {
        kind: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        other: Option<String>,
    },
    /// The far ends of the set's edges of the kinds, each once, of the types when given
    Out {
        kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        types: Option<Vec<String>>,
    },
    In {
        kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        types: Option<Vec<String>>,
    },
    /// The far ends of the set's edges of the kinds, out and in, each once, of the types when given
    Both {
        kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        types: Option<Vec<String>>,
    },
    /// Every entity the set reaches through the kinds, any number of edges deep or at most `depth`: nearest first, each
    /// once
    Descendants {
        kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        types: Option<Vec<String>>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        depth: Option<u32>,
    },
    Ancestors {
        kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        types: Option<Vec<String>>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        depth: Option<u32>,
    },
    /// A shortest path through the kinds from an entity of the set to `to`, at most `depth` edges long when given: its
    /// entities from start to `to`, both included, or none. Of paths as short, the one found first: from the set's
    /// earliest entity, then along each entity's edges kind by kind, out before in, each in place order
    PathTo {
        to: String,
        kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Direction>", optional))]
        direction: Direction,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        depth: Option<u32>,
    },
    /// The set's entities by personalized PageRank over the edges of the kinds between two of them, followed the
    /// direction's ways, highest score first, ties in the set's order; each score is a hit's, which `hits` answers.
    /// A walker follows an edge with probability `damping` and otherwise jumps back to a seed; with no `seeds` every
    /// entity is one, which is PageRank. A seed outside the set is ignored, and none inside it answers nothing. Rounds
    /// stop when the scores move by less than `tolerance` (their L1 distance) or after `iterations`, and a `hits` answer
    /// says which (`docs/roadmap/graph-algorithms.md`)
    Rank {
        kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Direction>", optional))]
        direction: Direction,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        seeds: Option<Vec<String>>,
        #[serde(default = "rank_damping")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<f64>", optional))]
        damping: f64,
        #[serde(default = "rank_iterations")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<u32>", optional))]
        iterations: u32,
        #[serde(default = "rank_tolerance")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<f64>", optional))]
        tolerance: f64,
    },
    /// By the field's value: booleans, numbers, timestamps, strings (by the schema's collation), anything else by its
    /// JSON, then a missing or null value; ties keep the query's order
    OrderBy {
        field: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        descending: bool,
    },
    Reverse,
    Limit {
        n: f64,
    },
    /// Each entity once, or with a field, the first entity of each value
    Distinct {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        field: Option<String>,
    },
    /// The entities after `id` in the order so far: a page's cursor, the last id of the page before
    After {
        id: String,
    },
    /// The `k` entities nearest `vector` in a type's vector index, nearest first, each by its nearest chunk
    Near {
        vector: Vec<f32>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        index: Option<String>,
        #[serde(default = "ten")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<u32>", optional))]
        k: u32,
    },
    /// The `k` entities nearest the entity `id`'s own embedding, nearest first, `id` itself left out: more like this
    Like {
        id: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        index: Option<String>,
        #[serde(default = "ten")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<u32>", optional))]
        k: u32,
    },
    /// A text search and a vector search of one type, their rankings fused (reciprocal rank fusion, 60): the `k` best
    Hybrid {
        query: String,
        vector: Vec<f32>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        text_index: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        vector_index: Option<String>,
        #[serde(default = "ten")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<u32>", optional))]
        k: u32,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<SearchOptions>", optional))]
        options: SearchOptions,
    },
    /// The entities a type's text index matches to `query`, most relevant first (`docs/architecture/search.md`). `index` names
    /// the index on a type with more than one
    Search {
        query: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        index: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<SearchOptions>", optional))]
        options: SearchOptions,
    },
    /// The entities whose text, in a field of a type's trigram index, holds `text`: exactly, or ignoring case. The
    /// set keeps its order; `index` names the index on a type with more than one
    Containing {
        text: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        index: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        case_insensitive: bool,
    },
    /// The entities whose text, in a field of a type's trigram index, matches `pattern`, in Rust's `regex` syntax,
    /// which has no look-around or backreferences. The set keeps its order
    Matching {
        pattern: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        index: Option<String>,
    },
}

/// How a search matches and ranks
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SearchOptions {
    /// The query's last word matches the terms it starts, for search as you type
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub prefix: bool,
    /// A word of five letters or more also matches a term one edit away, nine or more two
    #[serde(default = "yes")]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub fuzzy: bool,
    /// Any clause matching is enough, rather than every one
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub any: bool,
    /// Only these of the index's fields
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub fields: Option<Vec<String>>,
    /// A score halved for every `halfLife` milliseconds the field's time is before `now`
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub recency: Option<Recency>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self { prefix: false, fuzzy: true, any: false, fields: None, recency: None }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Recency {
    /// A number of milliseconds since the epoch, or a timestamp
    pub field: String,
    pub half_life: f64,
    /// The time ages are measured from; the read's clock when absent
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub now: Option<f64>,
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
        #[cfg_attr(feature = "codegen", ts(optional))]
        fields: Option<Vec<String>>,
    },
    /// The first entity's row, or with fields, its id and those of them it holds; `null` when there is none
    FirstRow {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        fields: Option<Vec<String>>,
    },
    /// Each edge of the kinds on the direction's sides of the set whose far end the query could reach, as a relation
    /// row, each relation once
    Edges {
        kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Direction>", optional))]
        direction: Direction,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        types: Option<Vec<String>>,
    },
    /// Each entity with how many edges of the kinds it has, counting those `edges` from it alone would answer
    Degrees {
        kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Direction>", optional))]
        direction: Direction,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        types: Option<Vec<String>>,
    },
    /// The set's weak components over the edges of the kinds between two of its entities, each edge joining its ends
    /// whichever way it points: each component's ids in the set's order, largest first, ties by the earlier
    /// first member; with `counts`, how many in place of the ids
    WeakComponents {
        kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        counts: bool,
    },
    /// The set's Leiden communities by modularity over the edges of the kinds between two of its entities, each edge
    /// joining its ends whichever way it points: groups more densely linked inside than out, each connected, each one's
    /// ids in the set's order, largest first, ties by the earlier first member. A higher `resolution` finds smaller
    /// communities, and 0 leaves each component whole. Leiden visits the entities in an order drawn from `seed`, so a
    /// seed answers the same communities for the same data on every platform; it stops after `iterations` rounds or
    /// when a round finds nothing better
    Communities {
        kinds: Vec<String>,
        #[serde(default = "communities_resolution")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<f64>", optional))]
        resolution: f64,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<u32>", optional))]
        seed: u32,
        #[serde(default = "rank_iterations")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<u32>", optional))]
        iterations: u32,
    },
    /// The ids by the field's value, in the order each value first appears; with `counts`, how many in place of the ids
    GroupBy {
        field: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        counts: bool,
    },
    /// Each entity with what a search found in it: its score, the index's terms it matched, and with `highlights`
    /// where they are in each field, `[element, start, end]` in UTF-16 units; with `fields`, its id and those fields
    Hits {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        fields: Option<Vec<String>>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        highlights: bool,
    },
}

/// A rank's defaults: the damping PageRank was defined with, and enough rounds that the scores settle within the
/// tolerance from any start, the L1 change after `n` rounds being at most 2 × 0.85ⁿ, below 1e-6 by the 90th
fn rank_damping() -> f64 {
    0.85
}

fn rank_iterations() -> u32 {
    100
}

fn rank_tolerance() -> f64 {
    1e-6
}

/// Newman's modularity as Leiden was published optimizing it
fn communities_resolution() -> f64 {
    1.0
}
