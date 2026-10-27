//! Vector indexes (`docs/architecture/search.md`, Embeddings): a type's text, split into chunks, embedded by the
//! caller, and kept as hannoy graphs in the environment's graph databases. Under a type and an index
//! (`docs/architecture/format.md`, Keys):
//!
//! - `p/q id chunk` → the embedding a caller wrote for a chunk: the model, the chunk text's hash, the vector. Primary,
//!   since no field derives it, and kept only while its chunk holds that text
//! - `d/qc id chunk` → each chunk of the entity's text: its hash and its text
//! - `d/qp id chunk` → that a chunk has no embedding of its current text under the index's model: the queue a caller's
//!   worker drains (`pendingEmbeddings`)
//! - `d/qi item`, `d/qm id chunk` → a graph item's chunk, and a chunk's item
//! - `d/qj item` → a graph change the unit makes when it commits
//! - `d/qn` → the index's graph: its number among the graphs, and its next item
//!
//! hannoy writes to its own databases, which a savepoint's undo doesn't reach, so a unit changes its graphs only as it
//! commits (`flush`), from the journal its writes left, which a rolled-back savepoint takes back with them. A read
//! inside a unit sees the graphs as they were when the unit began.

use std::collections::{BTreeMap, HashMap, HashSet};

use hannoy::distances::{Cosine, Euclidean};
use hannoy::{Reader, Writer};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rmpv::Value;
use roaring::RoaringBitmap;
use sha2::{Digest, Sha256};

use crate::error::{Error, ErrorCode, Result};
use crate::format::Prefix;
use crate::format::key::{Part, decode as decode_key};
use crate::schema::{Chunk, IndexDef, Metric, Schema};

use super::entities;
use super::ids;
use super::keys;
use super::unit::{Read, Txn};
use super::values;

/// A vector index on a type, as the writes keep it
#[derive(Debug, Clone)]
pub struct VectorIndex {
    pub name: String,
    pub from: Vec<String>,
    pub chunk: Chunk,
    pub dims: usize,
    pub metric: Metric,
    pub model: String,
}

/// The type's vector indexes
pub fn vector_indexes(schema: &Schema, entity_type: &str) -> Vec<VectorIndex> {
    let Some(def) = schema.types.get(entity_type) else { return Vec::new() };
    def.indexes
        .iter()
        .filter_map(|(name, index)| match index {
            IndexDef::Vector { from, chunk, dims, metric, model, .. } => Some(VectorIndex {
                name: name.clone(),
                from: from.clone(),
                chunk: *chunk,
                dims: *dims as usize,
                metric: *metric,
                model: model.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The vector index of a type a read names, or the type's only one: refusals as `text::resolve`'s
pub fn resolve(schema: &Schema, entity_type: &str, name: Option<&str>, via: &str) -> Result<VectorIndex> {
    if !schema.types.contains_key(entity_type) {
        return Err(Error::new(
            ErrorCode::UnknownType,
            format!("{via}: \"{entity_type}\" isn't a type the schema declares"),
        ));
    }
    let mut indexes = vector_indexes(schema, entity_type);
    match name {
        Some(name) => match indexes.iter().position(|i| i.name == name) {
            Some(at) => Ok(indexes.swap_remove(at)),
            None => {
                Err(Error::new(ErrorCode::UnknownIndex, format!("{via}: {entity_type} has no vector index \"{name}\"")))
            }
        },
        None if indexes.len() == 1 => Ok(indexes.remove(0)),
        None if indexes.is_empty() => {
            Err(Error::new(ErrorCode::UnknownIndex, format!("{via}: {entity_type} has no vector index")))
        }
        None => {
            let names: Vec<&str> = indexes.iter().map(|i| i.name.as_str()).collect();
            Err(Error::new(
                ErrorCode::UnknownIndex,
                format!("{via}: {entity_type} has the vector indexes {}: name the one to search", names.join(", ")),
            ))
        }
    }
}

/// A chunk's hash: what an embedding names the text it was made from by
pub fn hash_of(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The chunks of an entity's text: its `from` fields' strings, joined, split as the index says — whole, by paragraph
/// (blank lines), or by Markdown heading, each section with its heading line. Empty chunks are none
pub fn chunks_of(index: &VectorIndex, values: &[Option<Value>]) -> Vec<String> {
    let texts: Vec<&str> = values.iter().flat_map(|v| super::text::texts_of(v.as_ref())).collect();
    let joined = texts.join("\n\n");
    let mut out: Vec<String> = match index.chunk {
        Chunk::Whole => vec![joined],
        Chunk::Paragraph => {
            let mut paragraphs = Vec::new();
            let mut current = String::new();
            for line in joined.lines() {
                if line.trim().is_empty() {
                    paragraphs.push(std::mem::take(&mut current));
                } else {
                    if !current.is_empty() {
                        current.push('\n');
                    }
                    current.push_str(line);
                }
            }
            paragraphs.push(current);
            paragraphs
        }
        Chunk::Heading => {
            let mut sections = Vec::new();
            let mut current = String::new();
            for line in joined.lines() {
                if line.starts_with('#') && !current.trim().is_empty() {
                    sections.push(std::mem::take(&mut current));
                }
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(line);
            }
            sections.push(current);
            sections
        }
    };
    out.retain(|c| !c.trim().is_empty());
    out.iter_mut().for_each(|c| *c = c.trim().to_owned());
    out
}

fn field_values(r: &impl Read, index: &VectorIndex, id: &str) -> Result<Vec<Option<Value>>> {
    index.from.iter().map(|f| entities::get(r, id, f)).collect()
}

fn corrupt(what: &str) -> Error {
    Error::new(ErrorCode::Corrupted, format!("A vector index's {what} doesn't read"))
}

/// A stored embedding: its model, the hash of the text it embeds, the vector
pub struct Stored {
    pub model: String,
    pub hash: String,
    pub vector: Vec<f32>,
}

fn encode_vector(model: &str, hash: &str, vector: &[f32]) -> Vec<u8> {
    let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
    values::encode(&Value::Array(vec![model.into(), hash.into(), Value::Binary(bytes)]))
}

pub fn decode_vector(stored: &[u8]) -> Result<Stored> {
    let value = values::decode(stored)?;
    let items = value.as_array().filter(|i| i.len() == 3).ok_or_else(|| corrupt("embedding"))?;
    let model = items[0].as_str().ok_or_else(|| corrupt("embedding"))?.to_owned();
    let hash = items[1].as_str().ok_or_else(|| corrupt("embedding"))?.to_owned();
    let Value::Binary(bytes) = &items[2] else { return Err(corrupt("embedding")) };
    if bytes.len() % 4 != 0 {
        return Err(corrupt("embedding"));
    }
    let vector = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    Ok(Stored { model, hash, vector })
}

fn encode_chunk(hash: &str, text: &str) -> Vec<u8> {
    values::encode(&Value::Array(vec![hash.into(), text.into()]))
}

pub fn decode_chunk(stored: &[u8]) -> Result<(String, String)> {
    let value = values::decode(stored)?;
    let items = value.as_array().filter(|i| i.len() == 2).ok_or_else(|| corrupt("chunk"))?;
    Ok((
        items[0].as_str().ok_or_else(|| corrupt("chunk"))?.to_owned(),
        items[1].as_str().ok_or_else(|| corrupt("chunk"))?.to_owned(),
    ))
}

fn chunk_number(key: &[u8]) -> Result<u32> {
    match decode_key(key)?.pop() {
        Some(Part::Number(n)) => Ok(n as u32),
        _ => Err(corrupt("key")),
    }
}

/// The entity's stored chunks, by number: `(hash, text)`
fn stored_chunks(r: &impl Read, entity_type: &str, index: &str, id: &str) -> Result<BTreeMap<u32, (String, String)>> {
    let mut out = BTreeMap::new();
    for (key, value) in r.scan(&keys::vector_chunks_of(Prefix::VectorChunk, entity_type, index, id))? {
        out.insert(chunk_number(&key)?, decode_chunk(&value)?);
    }
    Ok(out)
}

fn item_of(r: &impl Read, entity_type: &str, index: &str, id: &str, chunk: u32) -> Result<Option<u32>> {
    r.get(&keys::vector_chunk_key(Prefix::VectorItemOf, entity_type, index, id, chunk))?
        .map(|b| values::decode(&b)?.as_u64().map(|n| n as u32).ok_or_else(|| corrupt("item")))
        .transpose()
}

/// A vector index's graph: its number among the graphs, and its next item, taking the smallest number no other
/// index's graph has when it has none yet
fn graph(t: &mut Txn<'_, '_>, entity_type: &str, index: &str) -> Result<(u16, u32)> {
    if let Some((number, next)) = graph_of(t, entity_type, index)? {
        return Ok((number, next));
    }
    let mut taken = HashSet::new();
    for (_, value) in t.scan(&keys::vector_graphs())? {
        if let Some(n) = values::decode(&value)?.as_array().and_then(|a| a.first()).and_then(Value::as_u64) {
            taken.insert(n as u16);
        }
    }
    let number = (0..=u16::MAX)
        .find(|n| !taken.contains(n))
        .ok_or_else(|| Error::new(ErrorCode::SchemaInvalid, "Every graph number is taken: 65,536 vector indexes"))?;
    put_graph(t, entity_type, index, number, 0)?;
    Ok((number, 0))
}

pub fn graph_of(r: &impl Read, entity_type: &str, index: &str) -> Result<Option<(u16, u32)>> {
    let Some(stored) = r.get(&keys::vector_graph(entity_type, index))? else { return Ok(None) };
    let value = values::decode(&stored)?;
    let items = value.as_array().filter(|i| i.len() == 2).ok_or_else(|| corrupt("graph"))?;
    Ok(Some((
        items[0].as_u64().ok_or_else(|| corrupt("graph"))? as u16,
        items[1].as_u64().ok_or_else(|| corrupt("graph"))? as u32,
    )))
}

fn put_graph(t: &mut Txn<'_, '_>, entity_type: &str, index: &str, number: u16, next: u32) -> Result<()> {
    t.put(&keys::vector_graph(entity_type, index), &values::encode(&Value::Array(vec![number.into(), next.into()])))
}

/// Puts a chunk in its index's graph: a new item, or its own again with its new vector, as the unit commits
fn add_item(t: &mut Txn<'_, '_>, entity_type: &str, index: &str, id: &str, chunk: u32) -> Result<()> {
    let item = match item_of(t, entity_type, index, id, chunk)? {
        Some(item) => item,
        None => {
            let (number, next) = graph(t, entity_type, index)?;
            put_graph(t, entity_type, index, number, next + 1)?;
            t.put(
                &keys::vector_item_key(Prefix::VectorItem, entity_type, index, next),
                &values::encode(&Value::Array(vec![id.into(), chunk.into()])),
            )?;
            t.put(
                &keys::vector_chunk_key(Prefix::VectorItemOf, entity_type, index, id, chunk),
                &values::encode(&next.into()),
            )?;
            next
        }
    };
    t.put(&keys::vector_item_key(Prefix::VectorJournal, entity_type, index, item), &values::encode(&true.into()))
}

/// Takes a chunk out of its index's graph, as the unit commits
fn remove_item(t: &mut Txn<'_, '_>, entity_type: &str, index: &str, id: &str, chunk: u32) -> Result<()> {
    let Some(item) = item_of(t, entity_type, index, id, chunk)? else { return Ok(()) };
    t.del(&keys::vector_item_key(Prefix::VectorItem, entity_type, index, item))?;
    t.del(&keys::vector_chunk_key(Prefix::VectorItemOf, entity_type, index, id, chunk))?;
    t.put(&keys::vector_item_key(Prefix::VectorJournal, entity_type, index, item), &values::encode(&false.into()))
}

/// Refiles the entity in each vector index of its type that embeds `field` (every one when `None`), from its fields
/// as they are now: a chunk whose text changed or is gone loses its embedding and its graph item, and a chunk with no
/// embedding of its text under the index's model is pending
pub fn refile(t: &mut Txn<'_, '_>, id: &str, field: Option<&str>) -> Result<()> {
    let entity_type = ids::type_of(id).to_owned();
    let indexes: Vec<VectorIndex> = vector_indexes(t.schema, &entity_type)
        .into_iter()
        .filter(|i| field.is_none_or(|f| i.from.iter().any(|name| name == f)))
        .collect();
    if indexes.is_empty() || entities::record(t, id)?.is_none() {
        return Ok(());
    }
    for index in &indexes {
        let now = chunks_of(index, &field_values(t, index, id)?);
        file(t, &entity_type, index, id, &now)?;
    }
    Ok(())
}

/// Takes the entity out of every vector index of its type, embeddings included, as destroying it does
pub fn remove(t: &mut Txn<'_, '_>, id: &str) -> Result<()> {
    let entity_type = ids::type_of(id).to_owned();
    for index in vector_indexes(t.schema, &entity_type) {
        file(t, &entity_type, &index, id, &[])?;
        for (key, _) in t.scan(&keys::vectors_of(&entity_type, &index.name, id))? {
            t.del(&key)?;
        }
    }
    Ok(())
}

/// Moves what an index holds of the entity from its stored chunks to `now`
fn file(t: &mut Txn<'_, '_>, entity_type: &str, index: &VectorIndex, id: &str, now: &[String]) -> Result<()> {
    let name = index.name.as_str();
    let before = stored_chunks(t, entity_type, name, id)?;
    for (n, (hash, _)) in &before {
        let kept = now.get(*n as usize).is_some_and(|text| hash_of(text) == *hash);
        if !kept {
            remove_item(t, entity_type, name, id, *n)?;
            t.del(&keys::vector_chunk_key(Prefix::VectorChunk, entity_type, name, id, *n))?;
            t.del(&keys::vector_chunk_key(Prefix::VectorPending, entity_type, name, id, *n))?;
            t.del(&keys::vector(entity_type, name, id, *n))?;
        }
    }
    for (n, text) in now.iter().enumerate() {
        let n = n as u32;
        let hash = hash_of(text);
        if before.get(&n).is_some_and(|(h, _)| *h == hash) {
            continue;
        }
        t.put(&keys::vector_chunk_key(Prefix::VectorChunk, entity_type, name, id, n), &encode_chunk(&hash, text))?;
        let embedded = match t.get(&keys::vector(entity_type, name, id, n))? {
            Some(stored) => {
                let stored = decode_vector(&stored)?;
                stored.hash == hash && stored.model == index.model && stored.vector.len() == index.dims
            }
            None => false,
        };
        if embedded {
            add_item(t, entity_type, name, id, n)?;
        } else {
            t.put(
                &keys::vector_chunk_key(Prefix::VectorPending, entity_type, name, id, n),
                &values::encode(&Value::Nil),
            )?;
        }
    }
    Ok(())
}

/// What writing an embedding did: stored it, or nothing, its chunk's text having changed since it was read
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Embedded {
    Stored,
    Stale,
}

/// Stores a caller's embedding of a chunk: refused for an index the entity's type lacks (`UNKNOWN_INDEX`), an entity
/// that isn't there (`NO_SUCH_ENTITY`), and a vector of other dimensions or another model's (`VALUE_INVALID`); an
/// embedding of text the chunk no longer holds is answered stale and stored nowhere
pub fn embed(
    t: &mut Txn<'_, '_>,
    id: &str,
    index: Option<&str>,
    chunk: u32,
    hash: &str,
    model: &str,
    vector: &[f32],
) -> Result<Embedded> {
    let entity_type = ids::type_of(id).to_owned();
    let index = resolve(t.schema, &entity_type, index, "embed")?;
    // An entity gone since its chunks were pending took them with it: the embedding is of text no longer held
    if entities::record(t, id)?.is_none() {
        return Ok(Embedded::Stale);
    }
    if vector.len() != index.dims {
        return Err(Error::new(
            ErrorCode::ValueInvalid,
            format!(
                "embed: {entity_type}.{} holds vectors of {} dimensions, and this one has {}",
                index.name,
                index.dims,
                vector.len()
            ),
        ));
    }
    if model != index.model {
        return Err(Error::new(
            ErrorCode::ValueInvalid,
            format!("embed: {entity_type}.{} is embedded by {}, and this vector by {model}", index.name, index.model),
        ));
    }
    check_vector("embed", &index, vector)?;
    let name = index.name.as_str();
    let current = t.get(&keys::vector_chunk_key(Prefix::VectorChunk, &entity_type, name, id, chunk))?;
    let Some(current) = current else { return Ok(Embedded::Stale) };
    if decode_chunk(&current)?.0 != hash {
        return Ok(Embedded::Stale);
    }
    t.put(&keys::vector(&entity_type, name, id, chunk), &encode_vector(model, hash, vector))?;
    t.del(&keys::vector_chunk_key(Prefix::VectorPending, &entity_type, name, id, chunk))?;
    add_item(t, &entity_type, name, id, chunk).map(|()| Embedded::Stored)
}

/// Stores an embedding as it was, unchecked, as a load does (`Load::Embedding`): when the chunk holds the text it embeds
/// under the index's model, the chunk is no longer pending and takes its place in the graph
pub fn restore(
    t: &mut Txn<'_, '_>,
    id: &str,
    index: &str,
    chunk: u32,
    model: &str,
    hash: &str,
    vector: &[f32],
) -> Result<()> {
    let entity_type = ids::type_of(id).to_owned();
    t.put(&keys::vector(&entity_type, index, id, chunk), &encode_vector(model, hash, vector))?;
    let current = t.get(&keys::vector_chunk_key(Prefix::VectorChunk, &entity_type, index, id, chunk))?;
    let filed = vector_indexes(t.schema, &entity_type).into_iter().find(|i| i.name == index);
    let embeds = match (current, filed) {
        (Some(current), Some(filed)) => {
            decode_chunk(&current)?.0 == hash && filed.model == model && filed.dims == vector.len()
        }
        _ => false,
    };
    if embeds {
        t.del(&keys::vector_chunk_key(Prefix::VectorPending, &entity_type, index, id, chunk))?;
        add_item(t, &entity_type, index, id, chunk)?;
    }
    Ok(())
}

/// The graph changes the unit's writes journaled, made in the graphs as it commits, each graph then built once
pub fn flush(t: &mut Txn<'_, '_>) -> Result<()> {
    let journal = t.scan(&Prefix::VectorJournal.key().to_vec())?;
    if journal.is_empty() {
        return Ok(());
    }
    let mut by_index: BTreeMap<(String, String), Vec<(u32, bool)>> = BTreeMap::new();
    for (key, value) in &journal {
        let mut parts = decode_key(key)?;
        let item = match parts.pop() {
            Some(Part::Number(n)) => n as u32,
            _ => return Err(corrupt("journal")),
        };
        let (Some(Part::String(index)), Some(Part::String(entity_type))) = (parts.pop(), parts.pop()) else {
            return Err(corrupt("journal"));
        };
        let added = values::decode(value)?.as_bool().unwrap_or(false);
        by_index.entry((entity_type, index)).or_default().push((item, added));
    }
    for (key, _) in &journal {
        t.del(key)?;
    }
    for ((entity_type, index), changes) in by_index {
        let Some(def) = vector_indexes(t.schema, &entity_type).into_iter().find(|i| i.name == index) else { continue };
        let Some((number, _)) = graph_of(t, &entity_type, &index)? else { continue };
        let mut adds = Vec::new();
        for (item, added) in &changes {
            if !added {
                continue;
            }
            let Some(chunk) = t.get(&keys::vector_item_key(Prefix::VectorItem, &entity_type, &index, *item))? else {
                continue;
            };
            let chunk = values::decode(&chunk)?;
            let parts = chunk.as_array().ok_or_else(|| corrupt("item"))?;
            let id = parts.first().and_then(Value::as_str).ok_or_else(|| corrupt("item"))?;
            let n = parts.get(1).and_then(Value::as_u64).ok_or_else(|| corrupt("item"))? as u32;
            if let Some(stored) = t.get(&keys::vector(&entity_type, &index, id, n))? {
                adds.push((*item, decode_vector(&stored)?.vector));
            }
        }
        let removes: Vec<u32> = changes.iter().filter(|(_, added)| !added).map(|(item, _)| *item).collect();
        apply(t, &def, number, &removes, &adds, false)?;
    }
    Ok(())
}

/// Removes and adds items in a graph and builds it, or with `clear`, empties it first
fn apply(
    t: &mut Txn<'_, '_>,
    def: &VectorIndex,
    number: u16,
    removes: &[u32],
    adds: &[(u32, Vec<f32>)],
    clear: bool,
) -> Result<()> {
    macro_rules! write {
        ($db:expr) => {{
            let Some(db) = $db else {
                return Err(Error::new(
                    ErrorCode::ReadOnly,
                    "This database has no graph databases to write vectors to",
                ));
            };
            let writer = Writer::new(db, number, def.dims);
            if clear {
                writer.clear(t.txn).map_err(graph_error)?;
            }
            for item in removes {
                writer.del_item(t.txn, *item).map_err(graph_error)?;
            }
            for (item, vector) in adds {
                writer.add_item(t.txn, *item, vector).map_err(graph_error)?;
            }
            let mut rng = StdRng::seed_from_u64(u64::from(number));
            writer.builder(&mut rng).build::<16, 32>(t.txn).map_err(graph_error)?;
        }};
    }
    match def.metric {
        Metric::L2 => write!(t.db.euclidean),
        _ => write!(t.db.cosine),
    }
    Ok(())
}

fn graph_error(e: hannoy::Error) -> Error {
    match e {
        hannoy::Error::Heed(e) => e.into(),
        other => Error::new(ErrorCode::Lmdb, format!("A vector graph failed: {other}")),
    }
}

/// Empties the graph numbered `number` in either distance's database, as dropping its index does: a number is one
/// index's whichever its distance
pub fn clear_graph(txn: &mut heed::RwTxn<'_>, db: super::super::store::Db, number: u16) -> Result<()> {
    if let Some(cosine) = db.cosine {
        Writer::new(cosine, number, 1).clear(txn).map_err(graph_error)?;
    }
    if let Some(euclidean) = db.euclidean {
        Writer::new(euclidean, number, 1).clear(txn).map_err(graph_error)?;
    }
    Ok(())
}

/// Builds every vector index's graph again from the stored embeddings, items numbered anew, as `rebuild()` does;
/// the chunks and pending markers are `derive`'s
pub fn rebuild_graphs(t: &mut Txn<'_, '_>) -> Result<()> {
    for prefix in [Prefix::VectorItem, Prefix::VectorItemOf, Prefix::VectorJournal, Prefix::VectorGraph] {
        for (key, _) in t.scan(&prefix.key().to_vec())? {
            t.del(&key)?;
        }
    }
    for db in [
        t.db.cosine.map(|d| d.remap_types::<heed::types::Bytes, heed::types::Bytes>()),
        t.db.euclidean.map(|d| d.remap_types::<heed::types::Bytes, heed::types::Bytes>()),
    ]
    .into_iter()
    .flatten()
    {
        db.clear(t.txn)?;
    }
    let types: Vec<String> = t.schema.types.keys().cloned().collect();
    for entity_type in types {
        for index in vector_indexes(t.schema, &entity_type) {
            let mut chunks: Vec<(String, u32)> = Vec::new();
            for (key, value) in t.scan(&keys::vector_index_range(Prefix::VectorChunk, &entity_type, &index.name))? {
                let mut parts = decode_key(&key)?;
                let n = match parts.pop() {
                    Some(Part::Number(n)) => n as u32,
                    _ => return Err(corrupt("key")),
                };
                let Some(Part::String(id)) = parts.pop() else { return Err(corrupt("key")) };
                let (hash, _) = decode_chunk(&value)?;
                if let Some(stored) = t.get(&keys::vector(&entity_type, &index.name, &id, n))? {
                    let stored = decode_vector(&stored)?;
                    if stored.hash == hash && stored.model == index.model && stored.vector.len() == index.dims {
                        chunks.push((id, n));
                    }
                }
            }
            for (id, n) in chunks {
                add_item(t, &entity_type, &index.name, &id, n)?;
            }
        }
    }
    flush(t)
}

/// What a vector index should hold of a type's entities, chunks and pending markers, from their fields and stored
/// embeddings, as `check()` compares
pub fn entries_into(
    r: &impl Read,
    entity_type: &str,
    index: &VectorIndex,
    ids: &[String],
    mut emit: impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>,
) -> Result<()> {
    for id in ids {
        for (n, text) in chunks_of(index, &field_values(r, index, id)?).iter().enumerate() {
            let n = n as u32;
            let hash = hash_of(text);
            emit(
                keys::vector_chunk_key(Prefix::VectorChunk, entity_type, &index.name, id, n),
                encode_chunk(&hash, text),
            )?;
            let embedded = match r.get(&keys::vector(entity_type, &index.name, id, n))? {
                Some(stored) => {
                    let stored = decode_vector(&stored)?;
                    stored.hash == hash && stored.model == index.model && stored.vector.len() == index.dims
                }
                None => false,
            };
            if !embedded {
                emit(
                    keys::vector_chunk_key(Prefix::VectorPending, entity_type, &index.name, id, n),
                    values::encode(&Value::Nil),
                )?;
            }
        }
    }
    Ok(())
}

/// One chunk waiting for its embedding
pub struct Pending {
    pub id: String,
    pub chunk: u32,
    pub hash: String,
    pub text: String,
}

/// Up to `limit` chunks of an index waiting for embeddings, in key order, and how many wait in all
pub fn pending(r: &impl Read, entity_type: &str, index: &str, limit: usize) -> Result<(Vec<Pending>, u64)> {
    let mut out = Vec::new();
    let mut count = 0u64;
    for (key, _) in r.scan(&keys::vector_index_range(Prefix::VectorPending, entity_type, index))? {
        count += 1;
        if out.len() >= limit {
            continue;
        }
        let mut parts = decode_key(&key)?;
        let n = match parts.pop() {
            Some(Part::Number(n)) => n as u32,
            _ => return Err(corrupt("key")),
        };
        let Some(Part::String(id)) = parts.pop() else { return Err(corrupt("key")) };
        let Some(chunk) = r.get(&keys::vector_chunk_key(Prefix::VectorChunk, entity_type, index, &id, n))? else {
            continue;
        };
        let (hash, text) = decode_chunk(&chunk)?;
        out.push(Pending { id, chunk: n, hash, text });
    }
    Ok((out, count))
}

/// Refuses a vector the index can't compare: one holding a number that isn't finite, or, under cosine, all zeros,
/// which has no direction
fn check_vector(what: &str, index: &VectorIndex, vector: &[f32]) -> Result<()> {
    if vector.iter().any(|f| !f.is_finite()) {
        return Err(Error::new(ErrorCode::ValueInvalid, format!("{what}: a vector holds only finite numbers")));
    }
    if index.metric == Metric::Cosine && vector.iter().all(|f| *f == 0.0) {
        return Err(Error::new(
            ErrorCode::ValueInvalid,
            format!("{what}: {} measures cosine distance, which a vector of zeros has none of", index.name),
        ));
    }
    Ok(())
}

/// The nearest chunks to `vector` in an index's graph, among `among`'s items when given, best `k` entities first, each
/// once with its nearest chunk's distance; and whether the graph held nothing more to ask for, which it did not when it
/// answered fewer items than it was asked for. Several chunks of one entity collapse into one answer, so fewer than `k`
/// entities does not by itself say the graph is exhausted
pub fn nearest(
    r: &impl Read,
    entity_type: &str,
    index: &VectorIndex,
    vector: &[f32],
    k: usize,
    among: Option<&RoaringBitmap>,
) -> Result<(Vec<(String, f32)>, bool)> {
    if vector.len() != index.dims {
        return Err(Error::new(
            ErrorCode::ValueInvalid,
            format!(
                "near: {entity_type}.{} holds vectors of {} dimensions, and the query has {}",
                index.name,
                index.dims,
                vector.len()
            ),
        ));
    }
    check_vector("near", index, vector)?;
    // The graph is read through hannoy, outside what a tracked read records: what it answers changes with the index's
    // items, which a commit adding or removing a vector writes, and with the embeddings, which re-embedding a chunk
    // writes in place
    r.note(&keys::vector_index_range(Prefix::VectorItem, entity_type, &index.name));
    r.note(&keys::vector_index_range(Prefix::Vector, entity_type, &index.name));
    let Some((number, _)) = graph_of(r, entity_type, &index.name)? else { return Ok((Vec::new(), true)) };
    let (txn, db) = r.raw();
    // Several chunks of one entity may come first: asking for more items than entities finds the k entities
    let wanted = (k * 4).max(16);
    macro_rules! search {
        ($db:expr, $d:ty) => {{
            let Some(db) = $db else { return Ok((Vec::new(), true)) };
            let reader = match Reader::<$d>::open(txn, number, db) {
                Ok(reader) => reader,
                Err(hannoy::Error::MissingMetadata(_)) => return Ok((Vec::new(), true)),
                Err(e) => return Err(graph_error(e)),
            };
            let mut query = reader.nns(wanted);
            query.ef_search(wanted.max(64));
            if let Some(among) = among {
                query.candidates(among);
            }
            query.by_vector(txn, vector).map_err(graph_error)?.nns
        }};
    }
    let found = match index.metric {
        Metric::L2 => search!(db.euclidean, Euclidean),
        _ => search!(db.cosine, Cosine),
    };
    // The graph held nothing past what it answered when it answered fewer items than asked, and only then is the
    // answer the last there is: unless `k` entities were found first, which leaves what it answered after unread
    let exhausted = found.len() < wanted;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (item, distance) in found {
        let Some(chunk) = r.get(&keys::vector_item_key(Prefix::VectorItem, entity_type, &index.name, item))? else {
            continue;
        };
        let chunk = values::decode(&chunk)?;
        let Some(id) = chunk.as_array().and_then(|a| a.first()).and_then(Value::as_str) else { continue };
        if seen.insert(id.to_owned()) {
            out.push((id.to_owned(), distance));
            if out.len() == k {
                return Ok((out, false));
            }
        }
    }
    Ok((out, exhausted))
}

/// The items of an index's graph that are chunks of these entities
pub fn items_of(r: &impl Read, entity_type: &str, index: &str, ids: &[String]) -> Result<RoaringBitmap> {
    let mut out = RoaringBitmap::new();
    for id in ids {
        for (_, item) in r.scan(&keys::vector_chunks_of(Prefix::VectorItemOf, entity_type, index, id))? {
            if let Some(item) = values::decode(&item)?.as_u64() {
                out.insert(item as u32);
            }
        }
    }
    Ok(out)
}

/// The entity's own embedding to search near: its first chunk's, when it has one
pub fn own_vector(r: &impl Read, entity_type: &str, index: &VectorIndex, id: &str) -> Result<Option<Vec<f32>>> {
    for (_, stored) in r.scan(&keys::vectors_of(entity_type, &index.name, id))? {
        let stored = decode_vector(&stored)?;
        if stored.model == index.model && stored.vector.len() == index.dims {
            return Ok(Some(stored.vector));
        }
    }
    Ok(None)
}

/// Every graph item's chunk must be one its index holds embedded, and every embedded chunk an item: what `check()`
/// reports, items being numbered as written rather than derived
pub fn check_items(r: &impl Read, schema: &Schema) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entity_type in schema.types.keys() {
        for index in vector_indexes(schema, entity_type) {
            let mut items: HashMap<(String, u32), u32> = HashMap::new();
            for (key, value) in r.scan(&keys::vector_index_range(Prefix::VectorItem, entity_type, &index.name))? {
                let item = chunk_number(&key)?;
                let chunk = values::decode(&value)?;
                let parts = chunk.as_array().ok_or_else(|| corrupt("item"))?;
                let id = parts.first().and_then(Value::as_str).ok_or_else(|| corrupt("item"))?.to_owned();
                let n = parts.get(1).and_then(Value::as_u64).ok_or_else(|| corrupt("item"))? as u32;
                if item_of(r, entity_type, &index.name, &id, n)? != Some(item) {
                    out.push(format!(
                        "{entity_type}.{} item {item} names {id} chunk {n}, which isn't that item",
                        index.name
                    ));
                }
                items.insert((id, n), item);
            }
            for (key, _) in r.scan(&keys::vector_index_range(Prefix::VectorChunk, entity_type, &index.name))? {
                let mut parts = decode_key(&key)?;
                let n = match parts.pop() {
                    Some(Part::Number(n)) => n as u32,
                    _ => return Err(corrupt("key")),
                };
                let Some(Part::String(id)) = parts.pop() else { return Err(corrupt("key")) };
                let pending =
                    r.contains(&keys::vector_chunk_key(Prefix::VectorPending, entity_type, &index.name, &id, n))?;
                let has_item = items.contains_key(&(id.clone(), n));
                if pending == has_item {
                    out.push(format!(
                        "{entity_type}.{} {id} chunk {n} is {} and {}",
                        index.name,
                        if pending { "pending" } else { "embedded" },
                        if has_item { "in the graph" } else { "not in the graph" }
                    ));
                }
            }
            if !r.scan(&keys::vector_index_range(Prefix::VectorJournal, entity_type, &index.name))?.is_empty() {
                out.push(format!("{entity_type}.{} holds graph changes no commit made", index.name));
            }
        }
    }
    Ok(out)
}

/// Files the entity at its place in one vector index, from its fields as they are: a build's step
pub fn refile_at(t: &mut Txn<'_, '_>, entity_type: &str, index: &VectorIndex, id: &str) -> Result<()> {
    let now = chunks_of(index, &field_values(t, index, id)?);
    file(t, entity_type, index, id, &now)
}

/// Deletes every derived key of a vector index of a type and its graph, keeping its stored embeddings
pub fn clear(t: &mut Txn<'_, '_>, entity_type: &str, index: &str) -> Result<()> {
    if let (Some(def), Some((number, _))) =
        (vector_indexes(t.schema, entity_type).into_iter().find(|i| i.name == index), graph_of(t, entity_type, index)?)
    {
        apply(t, &def, number, &[], &[], true)?;
    }
    for prefix in
        [Prefix::VectorChunk, Prefix::VectorPending, Prefix::VectorItem, Prefix::VectorItemOf, Prefix::VectorJournal]
    {
        for (key, _) in t.scan(&keys::vector_index_range(prefix, entity_type, index))? {
            t.del(&key)?;
        }
    }
    t.del(&keys::vector_graph(entity_type, index))
}

#[cfg(test)]
mod tests;
