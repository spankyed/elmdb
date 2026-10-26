//! Every key the engine writes, built in one place from the prefix table (`format::Prefix`)

use crate::format::Prefix;
use crate::format::key::KeyWriter;

/// The built-in value index on every type's short code, unique
pub const SHORT_CODE_INDEX: &str = "shortCode";
/// The built-in value index on every type's roles
pub const ROLE_INDEX: &str = "role";

fn with(prefix: Prefix, build: impl FnOnce(&mut KeyWriter)) -> Vec<u8> {
    let mut w = prefix.key();
    build(&mut w);
    w.to_vec()
}

pub fn value(id: &str, field: &str) -> Vec<u8> {
    with(Prefix::Value, |w| {
        w.string(id).string(field);
    })
}

pub fn values_of(id: &str) -> Vec<u8> {
    with(Prefix::Value, |w| {
        w.string(id);
    })
}

pub fn entity(id: &str) -> Vec<u8> {
    with(Prefix::Entity, |w| {
        w.string(id);
    })
}

pub fn relation(id: &str) -> Vec<u8> {
    with(Prefix::Relation, |w| {
        w.string(id);
    })
}

/// Every attribute of every entity
pub fn all_values() -> Vec<u8> {
    Prefix::Value.key().to_vec()
}

pub fn all_relations() -> Vec<u8> {
    Prefix::Relation.key().to_vec()
}

pub fn all_entities() -> Vec<u8> {
    Prefix::Entity.key().to_vec()
}

pub fn sequence(name: &str) -> Vec<u8> {
    with(Prefix::Sequence, |w| {
        w.string(name);
    })
}

/// A type's live entities (`trashed` false) or trashed ones, at a place
pub fn of_type(trashed: bool, entity_type: &str, seq: u64) -> Vec<u8> {
    with(if trashed { Prefix::Trashed } else { Prefix::Live }, |w| {
        w.string(entity_type).number(seq as f64);
    })
}

/// Every type's live entities (`trashed` false) or trashed ones
pub fn all_of_types(trashed: bool) -> Vec<u8> {
    with(if trashed { Prefix::Trashed } else { Prefix::Live }, |_| {})
}

pub fn type_range(trashed: bool, entity_type: &str) -> Vec<u8> {
    with(if trashed { Prefix::Trashed } else { Prefix::Live }, |w| {
        w.string(entity_type);
    })
}

pub fn count(entity_type: &str) -> Vec<u8> {
    with(Prefix::Count, |w| {
        w.string(entity_type);
    })
}

/// An index entry: the type, the index, the key's parts, and the entity's place
pub fn index_entry(entity_type: &str, index: &str, parts: &[crate::format::key::Part], seq: u64) -> Vec<u8> {
    with(Prefix::Index, |w| {
        w.string(entity_type).string(index);
        for p in parts {
            w.part(p);
        }
        w.number(seq as f64);
    })
}

/// Every entry of an index under these leading parts
pub fn index_range(entity_type: &str, index: &str, parts: &[crate::format::key::Part]) -> Vec<u8> {
    with(Prefix::Index, |w| {
        w.string(entity_type).string(index);
        for p in parts {
            w.part(p);
        }
    })
}

/// That an index of a type is built
pub fn index_built(entity_type: &str, index: &str) -> Vec<u8> {
    with(Prefix::IndexBuilt, |w| {
        w.string(entity_type).string(index);
    })
}

/// An edge out of `entity` (`outward`) or into it, of a kind, at the relation's place
pub fn edge(outward: bool, entity: &str, kind: &str, seq: u64) -> Vec<u8> {
    with(if outward { Prefix::Out } else { Prefix::In }, |w| {
        w.string(entity).string(kind).number(seq as f64);
    })
}

/// An entity's edges out or in, of one kind or of every kind
pub fn edges_of(outward: bool, entity: &str, kind: Option<&str>) -> Vec<u8> {
    with(if outward { Prefix::Out } else { Prefix::In }, |w| {
        w.string(entity);
        if let Some(kind) = kind {
            w.string(kind);
        }
    })
}

pub fn between(source: &str, kind: &str, target: &str, seq: u64) -> Vec<u8> {
    with(Prefix::Between, |w| {
        w.string(source).string(kind).string(target).number(seq as f64);
    })
}

pub fn between_range(source: &str, kind: &str, target: &str) -> Vec<u8> {
    with(Prefix::Between, |w| {
        w.string(source).string(kind).string(target);
    })
}

/// A text index's posting list for a term
pub fn text_term(entity_type: &str, index: &str, term: &str) -> Vec<u8> {
    with(Prefix::TextTerm, |w| {
        w.string(entity_type).string(index).string(term);
    })
}

/// Every term of a text index, in term order
pub fn text_terms(entity_type: &str, index: &str) -> Vec<u8> {
    with(Prefix::TextTerm, |w| {
        w.string(entity_type).string(index);
    })
}

/// The bytes every term of a text index starting with `prefix` starts with, for `Read::visit_starting`
pub fn text_terms_starting(entity_type: &str, index: &str, prefix: &str) -> Vec<u8> {
    with(Prefix::TextTerm, |w| {
        w.string(entity_type).string(index).string_prefix(prefix);
    })
}

/// Where a term is in one entity's fields
pub fn text_positions(entity_type: &str, index: &str, term: &str, seq: u64) -> Vec<u8> {
    with(Prefix::TextPositions, |w| {
        w.string(entity_type).string(index).string(term).number(seq as f64);
    })
}

/// What a text index holds of one entity
pub fn text_document(entity_type: &str, index: &str, seq: u64) -> Vec<u8> {
    with(Prefix::TextDocument, |w| {
        w.string(entity_type).string(index).number(seq as f64);
    })
}

/// A text index's statistics
pub fn text_stats(entity_type: &str, index: &str) -> Vec<u8> {
    with(Prefix::TextStats, |w| {
        w.string(entity_type).string(index);
    })
}

/// A stemming text index's posting list for a word as written
pub fn text_word(entity_type: &str, index: &str, word: &str) -> Vec<u8> {
    with(Prefix::TextWord, |w| {
        w.string(entity_type).string(index).string(word);
    })
}

/// Every word as written of a stemming text index, in word order
pub fn text_words(entity_type: &str, index: &str) -> Vec<u8> {
    with(Prefix::TextWord, |w| {
        w.string(entity_type).string(index);
    })
}

/// The bytes every word as written of a stemming text index starting with `prefix` starts with
pub fn text_words_starting(entity_type: &str, index: &str, prefix: &str) -> Vec<u8> {
    with(Prefix::TextWord, |w| {
        w.string(entity_type).string(index).string_prefix(prefix);
    })
}

/// The ranges of a text index's terms, positions, documents and words; its statistics are the one key `text_stats`
/// builds
pub fn text_index_ranges(entity_type: &str, index: &str) -> [Vec<u8>; 4] {
    let range = |prefix: Prefix| {
        with(prefix, |w| {
            w.string(entity_type).string(index);
        })
    };
    [range(Prefix::TextTerm), range(Prefix::TextPositions), range(Prefix::TextDocument), range(Prefix::TextWord)]
}

/// A chunk's stored embedding, the vectors of an entity's chunks, and of an index
pub fn vector(entity_type: &str, index: &str, id: &str, chunk: u32) -> Vec<u8> {
    with(Prefix::Vector, |w| {
        w.string(entity_type).string(index).string(id).number(f64::from(chunk));
    })
}

pub fn vectors_of(entity_type: &str, index: &str, id: &str) -> Vec<u8> {
    with(Prefix::Vector, |w| {
        w.string(entity_type).string(index).string(id);
    })
}

/// A key under a vector index's derived prefix, and its ranges: `(type, index, id, chunk)` for chunks, pending
/// markers and an item's place; `(type, index, item)` for items and journal entries
pub fn vector_chunk_key(prefix: Prefix, entity_type: &str, index: &str, id: &str, chunk: u32) -> Vec<u8> {
    with(prefix, |w| {
        w.string(entity_type).string(index).string(id).number(f64::from(chunk));
    })
}

pub fn vector_chunks_of(prefix: Prefix, entity_type: &str, index: &str, id: &str) -> Vec<u8> {
    with(prefix, |w| {
        w.string(entity_type).string(index).string(id);
    })
}

pub fn vector_item_key(prefix: Prefix, entity_type: &str, index: &str, item: u32) -> Vec<u8> {
    with(prefix, |w| {
        w.string(entity_type).string(index).number(f64::from(item));
    })
}

/// Every key of a vector index under one of its prefixes
pub fn vector_index_range(prefix: Prefix, entity_type: &str, index: &str) -> Vec<u8> {
    with(prefix, |w| {
        w.string(entity_type).string(index);
    })
}

/// A vector index's graph record: its number among the graphs, and its next item
pub fn vector_graph(entity_type: &str, index: &str) -> Vec<u8> {
    with(Prefix::VectorGraph, |w| {
        w.string(entity_type).string(index);
    })
}

/// Every vector index's graph record
pub fn vector_graphs() -> Vec<u8> {
    Prefix::VectorGraph.key().to_vec()
}
