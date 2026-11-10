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
    /// `d/rc kind` → a relation kind's count of relations
    RelationCount,
    /// `d/f type field` → that a type's entities have held the field
    FieldName,
    /// `d/x type index …` → a value index's entries
    Index,
    /// `d/xb type index` → that a value index is built
    IndexBuilt,
    /// `d/xw type index values seq` → an entity of the type without a value in a value index's last field, which is
    /// missing or null, under what its fields before that one hold
    IndexWithout,
    /// `d/xu type index seq` → an entity of the type holding, in a field of a value index, a value the index holds no
    /// one place for: a list, or a value it can't file
    IndexUnordered,
    /// `d/o entity kind seq` → an entity's outgoing edges
    Out,
    /// `d/n entity kind seq` → an entity's incoming edges
    In,
    /// `d/b source kind target seq` → the edges between two entities
    Between,
    /// `d/ft type index term` → a text index's term: the places of the entities holding it, each with the term's count
    /// in each of its fields, or that they are too many for one key and are in blocks
    TextTerm,
    /// `d/fb type index term start` → a block of the places of the entities holding a term held by too many for its own
    /// key, from `start` on
    TextBlock,
    /// `d/fd type index seq` → what a text index holds of one entity: its terms, each with its positions in each field,
    /// and its words as written
    TextDocument,
    /// `d/fl type index block` → each field's length in the entities of one block of places
    TextLengths,
    /// `d/fs type index` → a text index's statistics: its entities and each field's total length
    TextStats,
    /// `d/fw type index word` → a stemming text index's word as written: how many entities hold it
    TextWord,
    /// `d/g type index trigram block` → the places of the entities holding the trigram, case folded, in one block of places
    Trigram,
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
    /// `d/ra index entity` → a rank index's node: its estimate, residual and out-weight, and its place
    RankNode,
    /// `d/rk index score type seq` → a rank index's nodes in score order, highest first
    RankOrder,
    /// `d/rq index type seq` → a rank index's nodes whose residual is over its threshold, waiting to be pushed
    RankQueue,
    /// `d/rs index` → a rank index's totals: its nodes, their estimates and residuals summed, and how far it is built
    RankTotals,
    /// `d/h type changed place` → id: a type's entities in the order of the commits that last wrote them (`engine::changed`)
    Changed,
    /// `l/d sequence type id`: an entity a commit destroyed, kept for the last `TOMBSTONE_COMMITS` commits (`engine::changed`)
    Destroyed,
    /// `l/k name` → a reader's checkpoint, written in the unit whose work it records (`Unit::set_checkpoint`)
    Checkpoint,
}

impl Prefix {
    pub const ALL: [Prefix; 38] = [
        Prefix::Value,
        Prefix::Entity,
        Prefix::Relation,
        Prefix::Sequence,
        Prefix::Meta,
        Prefix::Live,
        Prefix::Trashed,
        Prefix::Count,
        Prefix::RelationCount,
        Prefix::FieldName,
        Prefix::Index,
        Prefix::IndexBuilt,
        Prefix::IndexWithout,
        Prefix::IndexUnordered,
        Prefix::Out,
        Prefix::In,
        Prefix::Between,
        Prefix::TextTerm,
        Prefix::TextBlock,
        Prefix::TextDocument,
        Prefix::TextLengths,
        Prefix::TextStats,
        Prefix::TextWord,
        Prefix::Trigram,
        Prefix::Vector,
        Prefix::VectorChunk,
        Prefix::VectorPending,
        Prefix::VectorItem,
        Prefix::VectorItemOf,
        Prefix::VectorJournal,
        Prefix::VectorGraph,
        Prefix::RankNode,
        Prefix::RankOrder,
        Prefix::RankQueue,
        Prefix::RankTotals,
        Prefix::Changed,
        Prefix::Destroyed,
        Prefix::Checkpoint,
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
            Prefix::RelationCount => "d/rc",
            Prefix::FieldName => "d/f",
            Prefix::Index => "d/x",
            Prefix::IndexBuilt => "d/xb",
            Prefix::IndexWithout => "d/xw",
            Prefix::IndexUnordered => "d/xu",
            Prefix::Out => "d/o",
            Prefix::In => "d/n",
            Prefix::Between => "d/b",
            Prefix::TextTerm => "d/ft",
            Prefix::TextBlock => "d/fb",
            Prefix::TextDocument => "d/fd",
            Prefix::TextLengths => "d/fl",
            Prefix::TextStats => "d/fs",
            Prefix::TextWord => "d/fw",
            Prefix::Trigram => "d/g",
            Prefix::Vector => "p/q",
            Prefix::VectorChunk => "d/qc",
            Prefix::VectorPending => "d/qp",
            Prefix::VectorItem => "d/qi",
            Prefix::VectorItemOf => "d/qm",
            Prefix::VectorJournal => "d/qj",
            Prefix::VectorGraph => "d/qn",
            Prefix::RankNode => "d/ra",
            Prefix::RankOrder => "d/rk",
            Prefix::RankQueue => "d/rq",
            Prefix::RankTotals => "d/rs",
            Prefix::Changed => "d/h",
            Prefix::Destroyed => "l/d",
            Prefix::Checkpoint => "l/k",
        }
    }

    pub const fn kind(self) -> PrefixKind {
        match self {
            Prefix::Value | Prefix::Entity | Prefix::Relation | Prefix::Sequence | Prefix::Vector => {
                PrefixKind::Primary
            }
            Prefix::Meta | Prefix::Destroyed | Prefix::Checkpoint => PrefixKind::Meta,
            _ => PrefixKind::Derived,
        }
    }

    /// The derived tags an earlier layout wrote under and none writes under now: `d/fp`, a term's positions in an
    /// entity. No prefix takes one again, and a rebuild of everything clears what a database written then still holds
    pub const RETIRED: [&'static str; 1] = ["d/fp"];

    /// The start of every key under a retired tag
    pub fn retired() -> impl Iterator<Item = Vec<u8>> {
        Self::RETIRED.into_iter().map(|tag| {
            let mut w = key::KeyWriter::new();
            w.string(tag);
            w.to_vec()
        })
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
    /// The last commit's sequence, a big-endian u64: a number that only increases for as long as the database exists
    pub const SEQUENCE: &str = "sequence";
    /// How many times every row has been replaced at once, a big-endian u64, absent for none
    pub const EPOCH: &str = "epoch";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_distinct_and_name_their_kind() {
        // A retired tag is no prefix's, or what a database kept under it would be read as that prefix's
        let mut tags: Vec<&str> = Prefix::ALL.iter().map(|p| p.tag()).chain(Prefix::RETIRED).collect();
        tags.sort();
        tags.dedup();
        assert_eq!(tags.len(), Prefix::ALL.len() + Prefix::RETIRED.len());
        assert!(Prefix::RETIRED.iter().all(|tag| tag.starts_with("d/")), "only derived data is left to a rebuild");
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
