//! How a database lays out its data (`docs/architecture/format.md`): one LMDB database, `DB_NAME`, of keys in the codec of `key`,
//! each beginning with one of the prefixes `Prefix` declares, and values in the spec `value` checks.

pub mod key;
pub mod value;

/// The format this version writes and reads, recorded as `m/format`
pub const FORMAT_VERSION: u32 = 1;

/// What `m/kind` holds in an elmdb database
pub const KIND: &str = "elmdb";

/// The LMDB database inside the environment that holds every key
pub const DB_NAME: &str = "elmdb";

/// The LMDB databases hannoy keeps the vector indexes' graphs in, one per distance: derived, rebuilt from `p/q`
pub const COSINE_GRAPHS: &str = "elmdb.graphs.cosine";
pub const EUCLIDEAN_GRAPHS: &str = "elmdb.graphs.euclidean";

/// Whether a prefix's keys are the truth, derived from it, or about the database itself
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixKind {
    Primary,
    /// Rebuildable from primary data: `rebuild()` deletes it and builds it again, and `check()` compares
    Derived,
    Meta,
}

/// Every key's first part. A key under no prefix here is never written
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Prefix {
    /// `p/v id field` → an attribute's value
    Value,
    /// `p/i id` → an entity's type, sequence and trash state
    Entity,
    /// `p/r relation` → a relation's row
    Relation,
    /// `p/s key` → a numbering sequence's next number
    Sequence,
    /// `m key` → the format, the kind, the schema and its hash
    Meta,
    /// `d/e type seq` → a type's live entities, in creation order
    Live,
    /// `d/t type seq` → a type's trashed entities
    Trashed,
    /// `d/c type` → a type's count of live entities
    Count,
    /// `d/x type index …` → a value index's entries
    Index,
    /// `d/xb type index` → that a value index is built
    IndexBuilt,
    /// `d/o entity kind seq` → an entity's outgoing edges
    Out,
    /// `d/n entity kind seq` → an entity's incoming edges
    In,
    /// `d/b source kind target seq` → the edges between two entities
    Between,
    /// `d/ft type index term` → a text index's posting list: the places of the entities holding the term
    TextTerm,
    /// `d/fp type index term seq` → where in the entity's fields the term is
    TextPositions,
    /// `d/fd type index seq` → what a text index holds of one entity: its id, each field's length and its terms
    TextDocument,
    /// `d/fs type index` → a text index's statistics: its entities and each field's total length
    TextStats,
    /// `p/q type index id chunk` → an embedding a caller wrote for a chunk: its model, the chunk's hash, the vector
    Vector,
    /// `d/qc type index id chunk` → a chunk of an entity's text a vector index embeds: its hash and its text
    VectorChunk,
    /// `d/qp type index id chunk` → that a chunk has no embedding of its current text under the index's model
    VectorPending,
    /// `d/qi type index item` → the entity and chunk a graph item is
    VectorItem,
    /// `d/qm type index id chunk` → the graph item a chunk is
    VectorItemOf,
    /// `d/qj type index item` → a graph change the unit makes as it commits: an item added or removed
    VectorJournal,
    /// `d/qn type index` → a vector index's graph: its number among the graphs, and its next item
    VectorGraph,
}

impl Prefix {
    pub const ALL: [Prefix; 24] = [
        Prefix::Value,
        Prefix::Entity,
        Prefix::Relation,
        Prefix::Sequence,
        Prefix::Meta,
        Prefix::Live,
        Prefix::Trashed,
        Prefix::Count,
        Prefix::Index,
        Prefix::IndexBuilt,
        Prefix::Out,
        Prefix::In,
        Prefix::Between,
        Prefix::TextTerm,
        Prefix::TextPositions,
        Prefix::TextDocument,
        Prefix::TextStats,
        Prefix::Vector,
        Prefix::VectorChunk,
        Prefix::VectorPending,
        Prefix::VectorItem,
        Prefix::VectorItemOf,
        Prefix::VectorJournal,
        Prefix::VectorGraph,
    ];

    /// The string the key's first part holds
    pub const fn tag(self) -> &'static str {
        match self {
            Prefix::Value => "p/v",
            Prefix::Entity => "p/i",
            Prefix::Relation => "p/r",
            Prefix::Sequence => "p/s",
            Prefix::Meta => "m",
            Prefix::Live => "d/e",
            Prefix::Trashed => "d/t",
            Prefix::Count => "d/c",
            Prefix::Index => "d/x",
            Prefix::IndexBuilt => "d/xb",
            Prefix::Out => "d/o",
            Prefix::In => "d/n",
            Prefix::Between => "d/b",
            Prefix::TextTerm => "d/ft",
            Prefix::TextPositions => "d/fp",
            Prefix::TextDocument => "d/fd",
            Prefix::TextStats => "d/fs",
            Prefix::Vector => "p/q",
            Prefix::VectorChunk => "d/qc",
            Prefix::VectorPending => "d/qp",
            Prefix::VectorItem => "d/qi",
            Prefix::VectorItemOf => "d/qm",
            Prefix::VectorJournal => "d/qj",
            Prefix::VectorGraph => "d/qn",
        }
    }

    pub const fn kind(self) -> PrefixKind {
        match self {
            Prefix::Value | Prefix::Entity | Prefix::Relation | Prefix::Sequence | Prefix::Vector => {
                PrefixKind::Primary
            }
            Prefix::Meta => PrefixKind::Meta,
            _ => PrefixKind::Derived,
        }
    }

    /// The prefix a tag names, if any
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.tag() == tag)
    }

    /// A key writer started with this prefix
    pub fn key(self) -> key::KeyWriter {
        let mut w = key::KeyWriter::new();
        w.string(self.tag());
        w
    }
}

/// The meta keys
pub mod meta {
    pub const FORMAT: &str = "format";
    pub const KIND: &str = "kind";
    pub const SCHEMA: &str = "schema";
    pub const SCHEMA_HASH: &str = "schemaHash";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_distinct_and_name_their_kind() {
        let mut tags: Vec<&str> = Prefix::ALL.iter().map(|p| p.tag()).collect();
        tags.sort();
        tags.dedup();
        assert_eq!(tags.len(), Prefix::ALL.len());
        for p in Prefix::ALL {
            assert_eq!(Prefix::from_tag(p.tag()), Some(p));
            let expected = match p.tag().split('/').next() {
                Some("p") => PrefixKind::Primary,
                Some("d") => PrefixKind::Derived,
                _ => PrefixKind::Meta,
            };
            assert_eq!(p.kind(), expected, "{} is under the wrong letter", p.tag());
        }
    }
}
