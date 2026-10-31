//! The schema's types: what a database declares (`docs/architecture/schema.md`). These are the definition; the TypeScript and the
//! JSON Schema are generated from them. Every default is a written value, so the canonical form (`Schema::canonical`)
//! writes every key.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The schema language this version reads, which a schema names as its `$schema`
pub const SCHEMA_LANGUAGE: &str = "https://elmdb.dev/schema/1.json";

/// What a database holds and the rules it keeps
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Schema {
    /// The schema language's URL, `SCHEMA_LANGUAGE`
    #[serde(rename = "$schema")]
    pub language: String,
    /// How strings sort: `root` (ICU's root collation) or a locale such as `en`
    #[serde(default = "root_collation")]
    #[cfg_attr(feature = "codegen", ts(as = "Option<String>", optional))]
    pub collation: String,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<BTreeMap<String, RoleDef>>", optional))]
    pub roles: BTreeMap<String, RoleDef>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<BTreeMap<String, RelationDef>>", optional))]
    pub relations: BTreeMap<String, RelationDef>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<BTreeMap<String, TypeDef>>", optional))]
    pub types: BTreeMap<String, TypeDef>,
}

fn root_collation() -> String {
    "root".to_owned()
}

/// A role an entity may be granted
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RoleDef {
    /// Who declared it, named in every error about it
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional = nullable))]
    pub declared_by: Option<String>,
}

/// A relation kind
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RelationDef {
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional = nullable))]
    pub declared_by: Option<String>,
    pub cardinality: Cardinality,
    /// A link that would close a cycle through this kind is refused
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub acyclic: bool,
    /// At most one edge of this kind between two entities; linking a pair again updates the edge's info
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub unique: bool,
    /// Each edge carries a weight, a number from 0 to 2^960 and 1 when a link gives none, which `rank` and
    /// `communities` read
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub weighted: bool,
    /// The type pairs an edge may join; `None` is any
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional = nullable))]
    pub endpoints: Option<Vec<Endpoint>>,
    /// What an edge's info holds
    #[serde(default = "any_info")]
    #[cfg_attr(feature = "codegen", ts(as = "Option<FieldDef>", optional))]
    pub info: FieldDef,
}

/// An edge's info when its kind declares none: anything, or nothing
pub(super) fn any_info() -> FieldDef {
    FieldDef { value: ValueType::Any, optional: true, nullable: true }
}

/// How many edges of a kind each end may have, from the source's side
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum Cardinality {
    OneToOne,
    /// A target has at most one source: a child has one parent
    OneToMany,
    /// A source has at most one target
    ManyToOne,
    ManyToMany,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
pub struct Endpoint {
    pub source: String,
    pub target: String,
}

/// An entity type
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TypeDef {
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional = nullable))]
    pub declared_by: Option<String>,
    /// The engine numbers each created row `<prefix>-<n>`
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional = nullable))]
    pub short_code: Option<ShortCodeDef>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<BTreeMap<String, FieldDef>>", optional))]
    pub fields: BTreeMap<String, FieldDef>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<AdditionalFields>", optional))]
    pub additional_fields: AdditionalFields,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<BTreeMap<String, IndexDef>>", optional))]
    pub indexes: BTreeMap<String, IndexDef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
pub struct ShortCodeDef {
    pub prefix: String,
}

/// Whether a write may set a field the type doesn't declare
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum AdditionalFields {
    #[default]
    Refuse,
    /// Stored as `any`
    Allow,
}

/// A field: its value's type, and whether it may be absent or null
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
pub struct FieldDef {
    #[serde(flatten)]
    pub value: ValueType,
    /// The row may lack it. A field that isn't optional is required
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub optional: bool,
    /// It may hold `null`
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub nullable: bool,
}

/// What a value holds
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ValueType {
    String,
    /// A finite double
    Number,
    /// A whole number within ±2^53
    Integer,
    Boolean,
    Bytes,
    /// One of the listed strings
    Enum {
        values: Vec<String>,
    },
    /// An entity's id held as a value, naming one of these types
    Ref {
        to: Vec<String>,
    },
    List {
        of: Box<ValueType>,
    },
    /// A map, opaque inside
    Object,
    /// Anything msgpack holds
    Any,
}

impl ValueType {
    /// The name `type` takes for it
    pub fn name(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Bytes => "bytes",
            Self::Enum { .. } => "enum",
            Self::Ref { .. } => "ref",
            Self::List { .. } => "list",
            Self::Object => "object",
            Self::Any => "any",
        }
    }

    /// Whether a value index can hold it: a scalar the key codec orders, a list of those, or `any`, of whose values
    /// the index holds the scalars (the rest are found by walking the type's rows)
    pub fn is_value_indexable(&self) -> bool {
        match self {
            Self::String
            | Self::Number
            | Self::Integer
            | Self::Boolean
            | Self::Enum { .. }
            | Self::Ref { .. }
            | Self::Any => true,
            Self::List { of } => of.is_value_indexable() && !matches!(**of, Self::List { .. }),
            Self::Bytes | Self::Object => false,
        }
    }

    /// Whether a text index can read it: a string, or a list of strings
    pub fn is_text(&self) -> bool {
        match self {
            Self::String => true,
            Self::List { of } => **of == Self::String,
            _ => false,
        }
    }
}

/// Derived data kept for a type's fields
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum IndexDef {
    /// Equality, ranges and order over one field, or several for a compound index
    Value {
        fields: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        unique: bool,
    },
    /// Ranked full-text search
    Text {
        fields: BTreeMap<String, TextField>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Analyzer>", optional))]
        analyzer: Analyzer,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional = nullable))]
        stem: Option<String>,
    },
    /// Substrings and regular expressions over text fields: each field's text filed by its trigrams, which narrow a search
    /// to the entities that can match before each is checked
    Trigram { fields: Vec<String> },
    /// Nearest neighbours of embeddings computed from text fields
    Vector {
        from: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Chunk>", optional))]
        chunk: Chunk,
        dims: u32,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Metric>", optional))]
        metric: Metric,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Precision>", optional))]
        precision: Precision,
        model: String,
    },
}

impl IndexDef {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Value { .. } => "value",
            Self::Text { .. } => "text",
            Self::Vector { .. } => "vector",
            Self::Trigram { .. } => "trigram",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
pub struct TextField {
    #[serde(default = "unit_weight")]
    #[cfg_attr(feature = "codegen", ts(as = "Option<f64>", optional))]
    pub weight: f64,
}

fn unit_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum Analyzer {
    /// Words, identifiers split on case and punctuation, case and accents folded
    #[default]
    Standard,
    /// Whole values, case folded
    Exact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum Chunk {
    #[default]
    Whole,
    Paragraph,
    /// Markdown sections
    Heading,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum Metric {
    #[default]
    Cosine,
    Dot,
    L2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS, schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum Precision {
    #[default]
    F32,
    F16,
    I8,
}

/// The fields the engine writes itself, which no type may declare
pub const RESERVED_FIELDS: [&str; 7] = ["id", "entityType", "shortCode", "createdAt", "updatedAt", "trashedAt", "role"];

/// The type names the engine's own ids are written with, which no schema may declare
pub const RESERVED_TYPES: [&str; 2] = ["Relation", "Sequence"];

/// The stemming languages a standard text index may name: Snowball's, by ISO 639-1 code
pub const STEM_LANGUAGES: [&str; 18] =
    ["ar", "da", "de", "el", "en", "es", "fi", "fr", "hu", "it", "nl", "no", "pt", "ro", "ru", "sv", "ta", "tr"];

/// The most dimensions a vector index takes
pub const MAX_DIMS: u32 = 4096;
