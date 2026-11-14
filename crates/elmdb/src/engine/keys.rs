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

/// A relation kind's count
pub fn relation_count(kind: &str) -> Vec<u8> {
    with(Prefix::RelationCount, |w| {
        w.string(kind);
    })
}

/// That a type's entities have held a field
pub fn field_name(entity_type: &str, field: &str) -> Vec<u8> {
    with(Prefix::FieldName, |w| {
        w.string(entity_type).string(field);
    })
}

/// The fields a type's entities have held
pub fn field_names(entity_type: &str) -> Vec<u8> {
    with(Prefix::FieldName, |w| {
        w.string(entity_type);
    })
}

/// The key a value index files entities under for these parts, which every key of it under them as leading parts
/// starts with
pub fn index_range(entity_type: &str, index: &str, parts: &[crate::format::key::Part]) -> Vec<u8> {
    with(Prefix::Index, |w| {
        w.string(entity_type).string(index);
        for p in parts {
            w.part(p);
        }
    })
}

/// The least key above every entry of an index under these leading parts
pub fn index_past(entity_type: &str, index: &str, parts: &[crate::format::key::Part]) -> Vec<u8> {
    let mut key = index_range(entity_type, index, parts);
    key.push(crate::format::key::TAG_HIGH);
    key
}

/// The least key an entry of an index for a string can have under these leading parts, which is above every entry
/// under them for null, a boolean or a number
pub fn index_strings(entity_type: &str, index: &str, parts: &[crate::format::key::Part]) -> Vec<u8> {
    with(Prefix::Index, |w| {
        w.string(entity_type).string(index);
        for part in parts {
            w.part(part);
        }
        w.string_prefix("");
    })
}

/// The key a value index files entities under besides its entries: those without a value in its last field
/// (`Prefix::IndexWithout`), those it doesn't order (`Prefix::IndexUnordered`), or those holding a long string there
/// (`Prefix::IndexLong`), each under the parts its fields before the last hold. The key holds their places, as a
/// value's does (`engine/places.rs`)
pub fn index_others(prefix: Prefix, entity_type: &str, index: &str, leading: &[crate::format::key::Part]) -> Vec<u8> {
    with(prefix, |w| {
        w.string(entity_type).string(index);
        for part in leading {
            w.part(part);
        }
    })
}

/// The ranges a value index's keys are in: its entries, the entities without a value, those whose value it doesn't
/// order and those holding a long string; that it is built is the one key `index_built` builds
pub fn value_index_ranges(entity_type: &str, index: &str) -> [Vec<u8>; 4] {
    [
        index_range(entity_type, index, &[]),
        index_others(Prefix::IndexWithout, entity_type, index, &[]),
        index_others(Prefix::IndexUnordered, entity_type, index, &[]),
        index_others(Prefix::IndexLong, entity_type, index, &[]),
    ]
}

/// The key the blocks of a value's places are under, for a value whose own key is `own`: the same parts under the
/// tag for the blocks of its kind, `d/x`'s, `d/xw`'s, `d/xu`'s or `d/xl`'s. A block's key is this and where it starts
/// (`place_under`)
pub fn index_blocks(own: &[u8]) -> Vec<u8> {
    let kinds = [
        (Prefix::Index, Prefix::IndexPlaces),
        (Prefix::IndexWithout, Prefix::IndexWithoutPlaces),
        (Prefix::IndexUnordered, Prefix::IndexUnorderedPlaces),
        (Prefix::IndexLong, Prefix::IndexLongPlaces),
    ];
    for (of, blocks) in kinds {
        if let Some(parts) = own.strip_prefix(of.key().to_vec().as_slice()) {
            let mut key = blocks.key().to_vec();
            key.extend_from_slice(parts);
            return key;
        }
    }
    unreachable!("a value index's key")
}

/// The least room a sort key is filed in: under values that leave it less, an index keeps no order of its strings
pub const LEAST_ORDER_ROOM: usize = 64;

/// What the keys of an index's order begin with, under one build and these leading parts
pub fn order_range(entity_type: &str, index: &str, build: u64, leading: &[crate::format::key::Part]) -> Vec<u8> {
    with(Prefix::IndexOrder, |w| {
        w.string(entity_type).string(index).number(build as f64);
        for part in leading {
            w.part(part);
        }
    })
}

/// What every key of an index's order begins with, whatever its build
pub fn order_of(entity_type: &str, index: &str) -> Vec<u8> {
    with(Prefix::IndexOrder, |w| {
        w.string(entity_type).string(index);
    })
}

/// How many bytes of a sort key a key of an index's order holds under these leading parts: what a key's length
/// leaves after what the key begins with, the sort key's tag and its end, the part saying whether it was cut, and
/// the longest string that isn't long (`values::long`) after them. It is one room for every string under the
/// parts, since keys cut at different lengths can't be ordered by their bytes. None when it is under
/// `LEAST_ORDER_ROOM`: long names or long leading values, under which no order is kept
pub fn order_room(entity_type: &str, index: &str, leading: &[crate::format::key::Part]) -> Option<usize> {
    let string = super::values::MAX_INDEXED_STRING + 2;
    let taken = order_range(entity_type, index, 0, leading).len() + 2 + 1 + string;
    super::values::MAX_KEY_BYTES.checked_sub(taken).filter(|room| *room >= LEAST_ORDER_ROOM)
}

/// The key that says an index has an entry for `string` under these leading parts, where `sort_key` orders it
pub fn order_string(
    entity_type: &str,
    index: &str,
    build: u64,
    leading: &[crate::format::key::Part],
    sort_key: &super::order::SortKey,
    string: &str,
) -> Vec<u8> {
    let mut key = order_range(entity_type, index, build, leading);
    let mut w = KeyWriter::new();
    w.bytes(&sort_key.bytes).bool(sort_key.cut).string(string);
    key.extend_from_slice(&w.to_vec());
    key
}

/// The key that says the entity at `place` holds a long string under these leading parts, where `sort_key` orders it
pub fn order_place(
    entity_type: &str,
    index: &str,
    build: u64,
    leading: &[crate::format::key::Part],
    sort_key: &super::order::SortKey,
    place: u64,
) -> Vec<u8> {
    let mut key = order_range(entity_type, index, build, leading);
    let mut w = KeyWriter::new();
    w.bytes(&sort_key.bytes).bool(sort_key.cut).number(place as f64);
    key.extend_from_slice(&w.to_vec());
    key
}

/// The marker of an index's order: the stamp its keys were made under, their build, and whether it is whole
pub fn order_marker(entity_type: &str, index: &str) -> Vec<u8> {
    with(Prefix::IndexOrderBuilt, |w| {
        w.string(entity_type).string(index);
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

/// A text index's term: the places of the entities holding it, or that they are in blocks
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

/// Every block of the places of the entities holding a term of a text index, in place order: a block's key is this
/// and the place it starts at (`place_under`)
pub fn text_blocks(entity_type: &str, index: &str, term: &str) -> Vec<u8> {
    with(Prefix::TextBlock, |w| {
        w.string(entity_type).string(index).string(term);
    })
}

/// The key of what is kept under `key` at a place: `key` with the place as its next part
pub fn place_under(key: &[u8], place: u64) -> Vec<u8> {
    crate::format::key::with_number(key, place as f64)
}

/// The place a key built by `place_under` ends with
pub fn place_of(key: &[u8]) -> Option<u64> {
    crate::format::key::last_number(key).map(|place| place as u64)
}

/// Each field's length in the entities of a text index in one block of places
pub fn text_lengths(entity_type: &str, index: &str, block: u64) -> Vec<u8> {
    with(Prefix::TextLengths, |w| {
        w.string(entity_type).string(index).number(block as f64);
    })
}

/// What a text index holds of one entity
pub fn text_document(entity_type: &str, index: &str, seq: u64) -> Vec<u8> {
    with(Prefix::TextDocument, |w| {
        w.string(entity_type).string(index).number(seq as f64);
    })
}

/// The places of the entities holding a trigram in a trigram index, in one block of places
pub fn trigram(entity_type: &str, index: &str, trigram: &str, block: u64) -> Vec<u8> {
    with(Prefix::Trigram, |w| {
        w.string(entity_type).string(index).string(trigram).number(block as f64);
    })
}

/// Every block of the places of the entities holding a trigram in a trigram index, in place order
pub fn trigram_places(entity_type: &str, index: &str, trigram: &str) -> Vec<u8> {
    with(Prefix::Trigram, |w| {
        w.string(entity_type).string(index).string(trigram);
    })
}

/// Every key of a trigram index of a type
pub fn trigram_index(entity_type: &str, index: &str) -> Vec<u8> {
    with(Prefix::Trigram, |w| {
        w.string(entity_type).string(index);
    })
}

/// A text index's statistics
pub fn text_stats(entity_type: &str, index: &str) -> Vec<u8> {
    with(Prefix::TextStats, |w| {
        w.string(entity_type).string(index);
    })
}

/// How many entities of a stemming text index hold a word as written
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

/// The ranges of a text index's terms, their blocks, its documents, their lengths and its words; its statistics are
/// the one key `text_stats` builds
pub fn text_index_ranges(entity_type: &str, index: &str) -> [Vec<u8>; 5] {
    let range = |prefix: Prefix| {
        with(prefix, |w| {
            w.string(entity_type).string(index);
        })
    };
    [
        range(Prefix::TextTerm),
        range(Prefix::TextBlock),
        range(Prefix::TextDocument),
        range(Prefix::TextLengths),
        range(Prefix::TextWord),
    ]
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

/// A rank index's node: `d/ra index entity`
pub fn rank_node(index: &str, entity: &str) -> Vec<u8> {
    with(Prefix::RankNode, |w| {
        w.string(index).string(entity);
    })
}

/// Every node of a rank index
pub fn rank_nodes(index: &str) -> Vec<u8> {
    with(Prefix::RankNode, |w| {
        w.string(index);
    })
}

/// A rank index's node in score order: the estimate negated, so the highest comes first, then the node's type in the
/// index's order and its place, as a plan over the index's types holds them
pub fn rank_order(index: &str, estimate: f64, type_at: u32, seq: u64) -> Vec<u8> {
    with(Prefix::RankOrder, |w| {
        w.string(index).number(-estimate).number(f64::from(type_at)).number(seq as f64);
    })
}

/// A rank index's nodes in score order
pub fn rank_orders(index: &str) -> Vec<u8> {
    with(Prefix::RankOrder, |w| {
        w.string(index);
    })
}

/// A rank index's node waiting to be pushed, in place order
pub fn rank_queued(index: &str, type_at: u32, seq: u64) -> Vec<u8> {
    with(Prefix::RankQueue, |w| {
        w.string(index).number(f64::from(type_at)).number(seq as f64);
    })
}

/// An entity's place in its type's change order
pub fn changed(entity_type: &str, changed: u64, place: u64) -> Vec<u8> {
    with(Prefix::Changed, |w| {
        w.string(entity_type).number(changed as f64).number(place as f64);
    })
}

/// A type's change order
pub fn changed_of_type(entity_type: &str) -> Vec<u8> {
    with(Prefix::Changed, |w| {
        w.string(entity_type);
    })
}

/// A tombstone of an entity a commit destroyed
pub fn destroyed(sequence: u64, entity_type: &str, id: &str) -> Vec<u8> {
    with(Prefix::Destroyed, |w| {
        w.number(sequence as f64).string(entity_type).string(id);
    })
}

/// The first tombstone at or after `sequence`
pub fn destroyed_through(sequence: u64) -> Vec<u8> {
    with(Prefix::Destroyed, |w| {
        w.number(sequence as f64);
    })
}

/// Every tombstone
pub fn destroyed_prefix() -> Vec<u8> {
    with(Prefix::Destroyed, |_| {})
}

/// The checkpoint named `name`
pub fn checkpoint(name: &str) -> Vec<u8> {
    with(Prefix::Checkpoint, |w| {
        w.string(name);
    })
}

/// A rank index's nodes waiting to be pushed
pub fn rank_queue(index: &str) -> Vec<u8> {
    with(Prefix::RankQueue, |w| {
        w.string(index);
    })
}

/// A rank index's totals
pub fn rank_totals(index: &str) -> Vec<u8> {
    with(Prefix::RankTotals, |w| {
        w.string(index);
    })
}
