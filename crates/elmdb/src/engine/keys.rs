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
