//! Text indexes (`docs/architecture/search.md`): a type's text fields as terms, kept by every write that changes one, in the same
//! unit. Under a type and an index (`docs/architecture/format.md`, Keys):
//!
//! - `d/ft term` → the term's posting list, the places (`seq`) of the entities holding it, as a roaring bitmap
//! - `d/fp term seq` → where the entity holds it: each field's index among the index's fields, and its positions
//! - `d/fd seq` → the entity's document: its id, each field's length in words, and its terms
//! - `d/fs` → the index's statistics: how many documents, and each field's total length
//! - `d/fw word` → a stemming index's words as written, each the places of the entities holding it: where search as
//!   you type and suggestions find what a word being typed starts, which its stem may not (`runni` starts `running`,
//!   held as `run`)
//!
//! A field holding a list of strings is its elements' terms, each element's positions starting `ELEMENT_GAP` past
//! the last one's, so no phrase spans two elements. A value that is neither is no text, and a term longer than
//! `MAX_TERM_BYTES` isn't held.

pub mod analyze;
pub mod labels;
pub mod search;

use std::collections::{BTreeMap, BTreeSet};

use rmpv::Value;
use roaring::RoaringTreemap;

use crate::error::{Error, ErrorCode, Result};
use crate::schema::{IndexDef, Schema};

use super::entities;
use super::ids;
use super::keys;
use super::unit::{Read, Txn};
use super::values;
use analyze::Analyzer;

/// How far past an element's last position the next element of a list field starts
pub const ELEMENT_GAP: u32 = 64;

/// The longest term a text index holds, in bytes: a key is at most 511 bytes, and no word worth finding is longer.
/// A longer one takes its place in the text, so the words around it are as far apart, and is matched by nothing
pub const MAX_TERM_BYTES: usize = 128;

/// A text index on a type, as the writes keep it
pub struct TextIndex {
    pub name: String,
    /// Its fields in name order, each with its weight; a field's place here is its index in the stored positions
    pub fields: Vec<(String, f64)>,
    pub analyzer: Analyzer,
}

/// The type's text indexes
pub fn text_indexes(schema: &Schema, entity_type: &str) -> Vec<TextIndex> {
    let Some(def) = schema.types.get(entity_type) else { return Vec::new() };
    def.indexes
        .iter()
        .filter_map(|(name, index)| match index {
            IndexDef::Text { fields, analyzer, stem } => Some(TextIndex {
                name: name.clone(),
                fields: fields.iter().map(|(f, d)| (f.clone(), d.weight)).collect(),
                analyzer: Analyzer::new(*analyzer, stem.as_deref()),
            }),
            _ => None,
        })
        .collect()
}

/// The text index of a type a search names, or the type's only one when it names none: `UNKNOWN_TYPE` for a type
/// the schema doesn't declare, `UNKNOWN_INDEX` for a name that is no text index of it, and for none named on a type
/// with no text index or with several
pub fn resolve(schema: &Schema, entity_type: &str, name: Option<&str>, via: &str) -> Result<TextIndex> {
    if !schema.types.contains_key(entity_type) {
        return Err(Error::new(
            ErrorCode::UnknownType,
            format!("{via}: \"{entity_type}\" isn't a type the schema declares"),
        ));
    }
    let mut indexes = text_indexes(schema, entity_type);
    match name {
        Some(name) => match indexes.iter().position(|i| i.name == name) {
            Some(at) => Ok(indexes.swap_remove(at)),
            None => {
                Err(Error::new(ErrorCode::UnknownIndex, format!("{via}: {entity_type} has no text index \"{name}\"")))
            }
        },
        None if indexes.len() == 1 => Ok(indexes.remove(0)),
        None if indexes.is_empty() => {
            Err(Error::new(ErrorCode::UnknownIndex, format!("{via}: {entity_type} has no text index")))
        }
        None => {
            let names: Vec<&str> = indexes.iter().map(|i| i.name.as_str()).collect();
            Err(Error::new(
                ErrorCode::UnknownIndex,
                format!("{via}: {entity_type} has the text indexes {}: name the one to search", names.join(", ")),
            ))
        }
    }
}

/// One entity as a text index holds it
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Document {
    /// Each field's length in words, in the index's field order
    pub lengths: Vec<u32>,
    /// Each term, with the fields and positions it is at: `(field, positions)` in field order
    pub terms: BTreeMap<String, Vec<(u32, Vec<u32>)>>,
    /// The words as written its terms were read from, when the index stems; none when every term is its word
    pub words: BTreeSet<String>,
}

impl Document {
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

/// The texts a field's value holds: a string, or each string of a list
/// `texts_of`, each with its element's place in the value: 0 for a string, its index in a list, where a value that
/// isn't text still holds a place
pub fn texts_at(value: Option<&Value>) -> Vec<(usize, &str)> {
    match value {
        Some(Value::String(s)) => s.as_str().into_iter().map(|s| (0, s)).collect(),
        Some(Value::Array(items)) => items.iter().enumerate().filter_map(|(i, v)| Some((i, v.as_str()?))).collect(),
        _ => Vec::new(),
    }
}

pub fn texts_of(value: Option<&Value>) -> Vec<&str> {
    match value {
        Some(Value::String(s)) => s.as_str().into_iter().collect(),
        Some(Value::Array(items)) => items.iter().filter_map(|v| v.as_str()).collect(),
        _ => Vec::new(),
    }
}

/// What the index holds of an entity, from its fields' values in field order
pub fn document_of(index: &TextIndex, values: &[Option<Value>]) -> Document {
    let mut doc = Document { lengths: vec![0; index.fields.len()], terms: BTreeMap::new(), words: BTreeSet::new() };
    let stems = index.analyzer.stems();
    for (field, value) in values.iter().enumerate() {
        let mut base = 0u32;
        for text in texts_of(value.as_ref()) {
            let terms = index.analyzer.terms(text);
            let mut last = None;
            for term in &terms {
                last = Some(term.position);
                if term.text.len() > MAX_TERM_BYTES {
                    continue;
                }
                if stems && term.word.len() <= MAX_TERM_BYTES {
                    doc.words.insert(term.word.clone());
                }
                let position = base + term.position;
                let places = doc.terms.entry(term.text.clone()).or_default();
                match places.last_mut() {
                    Some((f, positions)) if *f == field as u32 => positions.push(position),
                    _ => places.push((field as u32, vec![position])),
                }
            }
            if let Some(last) = last {
                // A field's length is its words: a code's whole term shares its first part's place
                doc.lengths[field] += last + 1;
                base += last + 1 + ELEMENT_GAP;
            }
        }
    }
    doc
}

/// The entity's fields an index reads, in its field order
fn field_values(r: &impl Read, index: &TextIndex, id: &str) -> Result<Vec<Option<Value>>> {
    index.fields.iter().map(|(f, _)| entities::get(r, id, f)).collect()
}

fn corrupt(what: &str) -> Error {
    Error::new(ErrorCode::Corrupted, format!("A text index's {what} doesn't read"))
}

// The stored forms, written and read only here, so `derive` writes exactly what the writes do

pub fn encode_postings(bitmap: &RoaringTreemap) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(bitmap.serialized_size());
    bitmap.serialize_into(&mut bytes).expect("writing to a Vec doesn't fail");
    values::encode(&Value::Binary(bytes))
}

pub fn decode_postings(stored: &[u8]) -> Result<RoaringTreemap> {
    match values::decode(stored)? {
        Value::Binary(bytes) => RoaringTreemap::deserialize_from(bytes.as_slice()).map_err(|_| corrupt("posting list")),
        _ => Err(corrupt("posting list")),
    }
}

fn encode_places(places: &[(u32, Vec<u32>)]) -> Vec<u8> {
    values::encode(&Value::Array(
        places
            .iter()
            .map(|(field, positions)| {
                Value::Array(vec![(*field).into(), Value::Array(positions.iter().map(|p| (*p).into()).collect())])
            })
            .collect(),
    ))
}

pub fn decode_places(stored: &[u8]) -> Result<Vec<(u32, Vec<u32>)>> {
    let value = values::decode(stored)?;
    let items = value.as_array().ok_or_else(|| corrupt("positions"))?;
    items
        .iter()
        .map(|item| {
            let pair = item.as_array().filter(|p| p.len() == 2).ok_or_else(|| corrupt("positions"))?;
            let field = pair[0].as_u64().ok_or_else(|| corrupt("positions"))? as u32;
            let positions = pair[1]
                .as_array()
                .ok_or_else(|| corrupt("positions"))?
                .iter()
                .map(|p| p.as_u64().map(|p| p as u32).ok_or_else(|| corrupt("positions")))
                .collect::<Result<_>>()?;
            Ok((field, positions))
        })
        .collect()
}

/// A document's stored record: the entity's id, each field's length, its terms, its words as written
fn encode_document(id: &str, doc: &Document) -> Vec<u8> {
    values::encode(&Value::Array(vec![
        id.into(),
        Value::Array(doc.lengths.iter().map(|l| (*l).into()).collect()),
        Value::Array(doc.terms.keys().map(|t| t.as_str().into()).collect()),
        Value::Array(doc.words.iter().map(|w| w.as_str().into()).collect()),
    ]))
}

/// A stored document: the entity's id, each field's length, its terms, and its words as written, in order
pub struct StoredDocument {
    pub id: String,
    pub lengths: Vec<u32>,
    pub terms: Vec<String>,
    pub words: Vec<String>,
}

pub fn decode_document(stored: &[u8]) -> Result<StoredDocument> {
    let value = values::decode(stored)?;
    let items = value.as_array().filter(|i| i.len() == 4).ok_or_else(|| corrupt("document"))?;
    let id = items[0].as_str().ok_or_else(|| corrupt("document"))?.to_owned();
    let lengths = items[1]
        .as_array()
        .ok_or_else(|| corrupt("document"))?
        .iter()
        .map(|l| l.as_u64().map(|l| l as u32).ok_or_else(|| corrupt("document")))
        .collect::<Result<_>>()?;
    let strings = |item: &Value| -> Result<Vec<String>> {
        item.as_array()
            .ok_or_else(|| corrupt("document"))?
            .iter()
            .map(|t| t.as_str().map(str::to_owned).ok_or_else(|| corrupt("document")))
            .collect()
    };
    Ok(StoredDocument { id, lengths, terms: strings(&items[2])?, words: strings(&items[3])? })
}

/// A text index's statistics: its documents, and each field's total length
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub documents: u64,
    pub lengths: Vec<u64>,
}

fn encode_stats(stats: &Stats) -> Vec<u8> {
    values::encode(&Value::Array(vec![
        stats.documents.into(),
        Value::Array(stats.lengths.iter().map(|l| (*l).into()).collect()),
    ]))
}

pub fn stats(r: &impl Read, entity_type: &str, index: &TextIndex) -> Result<Stats> {
    stats_from(r.get(&keys::text_stats(entity_type, &index.name))?.as_deref(), index)
}

/// The statistics an index's stored record holds, or none yet
fn stats_from(stored: Option<&[u8]>, index: &TextIndex) -> Result<Stats> {
    let Some(stored) = stored else {
        return Ok(Stats { documents: 0, lengths: vec![0; index.fields.len()] });
    };
    let value = values::decode(stored)?;
    let items = value.as_array().filter(|i| i.len() == 2).ok_or_else(|| corrupt("statistics"))?;
    let documents = items[0].as_u64().ok_or_else(|| corrupt("statistics"))?;
    let mut lengths: Vec<u64> = items[1]
        .as_array()
        .ok_or_else(|| corrupt("statistics"))?
        .iter()
        .map(|l| l.as_u64().ok_or_else(|| corrupt("statistics")))
        .collect::<Result<_>>()?;
    lengths.resize(index.fields.len(), 0);
    Ok(Stats { documents, lengths })
}

pub fn document(r: &impl Read, entity_type: &str, index: &str, seq: u64) -> Result<Option<StoredDocument>> {
    r.get(&keys::text_document(entity_type, index, seq))?.map(|b| decode_document(&b)).transpose()
}

/// Refiles the entity at `seq` (none for one not stored) in each text index of its type covering `field`, which now
/// holds `written`: the terms it no longer holds lose it, the terms it now holds gain it, and the statistics follow
pub fn refile(t: &mut Txn<'_, '_>, id: &str, field: &str, seq: Option<u64>, written: Option<&Value>) -> Result<()> {
    let Some(seq) = seq else { return Ok(()) };
    let entity_type = ids::type_of(id).to_owned();
    for index in
        text_indexes(t.schema, &entity_type).into_iter().filter(|i| i.fields.iter().any(|(name, _)| name == field))
    {
        let values = index
            .fields
            .iter()
            .map(|(f, _)| if f == field { Ok(written.cloned()) } else { entities::get(t, id, f) })
            .collect::<Result<Vec<_>>>()?;
        file(t, &entity_type, &index, id, seq, &document_of(&index, &values))?;
    }
    Ok(())
}

/// Files one entity at its place in one text index, from its fields as they are: what a build does for an entity
/// written before the index was declared, and nothing for one a write has already filed
pub fn refile_at(t: &mut Txn<'_, '_>, entity_type: &str, index: &TextIndex, id: &str, seq: u64) -> Result<()> {
    let now = document_of(index, &field_values(t, index, id)?);
    file(t, entity_type, index, id, seq, &now)
}

/// Takes the entity out of every text index of its type, as destroying it does
pub fn remove(t: &mut Txn<'_, '_>, id: &str) -> Result<()> {
    let entity_type = ids::type_of(id).to_owned();
    let Some(seq) = entities::record(t, id)?.map(|r| r.seq) else { return Ok(()) };
    for index in text_indexes(t.schema, &entity_type) {
        file(t, &entity_type, &index, id, seq, &Document::default())?;
    }
    Ok(())
}

/// Moves what the index holds of the entity at `seq` from its stored document to `now`
fn file(t: &mut Txn<'_, '_>, entity_type: &str, index: &TextIndex, id: &str, seq: u64, now: &Document) -> Result<()> {
    let name = index.name.as_str();
    // Each key read here is replaced with what was read, so the unit's undo log reads none of them again
    let document_key = keys::text_document(entity_type, name, seq);
    let stored_document = t.get(&document_key)?;
    let before = stored_document.as_deref().map(decode_document).transpose()?;
    if before.is_none() && now.is_empty() {
        return Ok(());
    }
    let before_terms: Vec<String> = before.as_ref().map(|d| d.terms.clone()).unwrap_or_default();
    for term in before_terms.iter().filter(|term| !now.terms.contains_key(*term)) {
        let key = keys::text_term(entity_type, name, term);
        if let Some(stored) = t.get(&key)? {
            let mut postings = decode_postings(&stored)?;
            postings.remove(seq);
            if postings.is_empty() {
                t.del_over(&key, Some(&stored))?
            } else {
                t.put_over(&key, &encode_postings(&postings), Some(&stored))?
            }
        }
        t.del(&keys::text_positions(entity_type, name, term, seq))?;
    }
    for (term, places) in &now.terms {
        if before_terms.binary_search(term).is_err() {
            let key = keys::text_term(entity_type, name, term);
            let stored = t.get(&key)?;
            let mut postings = stored.as_deref().map(decode_postings).transpose()?.unwrap_or_default();
            postings.insert(seq);
            t.put_over(&key, &encode_postings(&postings), stored.as_deref())?;
        }
        let key = keys::text_positions(entity_type, name, term, seq);
        let encoded = encode_places(places);
        let held = t.get(&key)?;
        if held.as_deref() != Some(encoded.as_slice()) {
            t.put_over(&key, &encoded, held.as_deref())?;
        }
    }
    let before_words: Vec<String> = before.as_ref().map(|d| d.words.clone()).unwrap_or_default();
    for word in before_words.iter().filter(|word| !now.words.contains(*word)) {
        let key = keys::text_word(entity_type, name, word);
        if let Some(stored) = t.get(&key)? {
            let mut postings = decode_postings(&stored)?;
            postings.remove(seq);
            if postings.is_empty() {
                t.del_over(&key, Some(&stored))?
            } else {
                t.put_over(&key, &encode_postings(&postings), Some(&stored))?
            }
        }
    }
    for word in now.words.iter().filter(|word| before_words.binary_search(word).is_err()) {
        let key = keys::text_word(entity_type, name, word);
        let stored = t.get(&key)?;
        let mut postings = stored.as_deref().map(decode_postings).transpose()?.unwrap_or_default();
        postings.insert(seq);
        t.put_over(&key, &encode_postings(&postings), stored.as_deref())?;
    }
    let stats_key = keys::text_stats(entity_type, name);
    let stored_stats = t.get(&stats_key)?;
    let mut stats = stats_from(stored_stats.as_deref(), index)?;
    if let Some(before) = &before {
        stats.documents -= 1;
        for (total, l) in stats.lengths.iter_mut().zip(&before.lengths) {
            *total -= u64::from(*l);
        }
    }
    if !now.is_empty() {
        stats.documents += 1;
        for (total, l) in stats.lengths.iter_mut().zip(&now.lengths) {
            *total += u64::from(*l);
        }
        t.put_over(&document_key, &encode_document(id, now), stored_document.as_deref())?;
    } else {
        t.del_over(&document_key, stored_document.as_deref())?;
    }
    if stats.documents == 0 {
        t.del_over(&stats_key, stored_stats.as_deref())
    } else {
        t.put_over(&stats_key, &encode_stats(&stats), stored_stats.as_deref())
    }
}

pub fn entries_into(
    r: &impl Read,
    entity_type: &str,
    index: &TextIndex,
    of: &[(u64, String)],
    mut emit: impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>,
) -> Result<()> {
    let mut entries = Entries::new(entity_type, index)?;
    for (seq, id) in of {
        entries.entity(r, *seq, id, &mut emit)?;
    }
    entries.finish(emit)
}

/// Accumulate term/document memberships on disk; only one document and one final posting bitmap are held at a time.
pub(super) struct Entries<'a> {
    entity_type: &'a str,
    index: &'a TextIndex,
    memberships: super::derive::sorted::Scratch,
    stats: Stats,
}

impl<'a> Entries<'a> {
    pub(super) fn new(entity_type: &'a str, index: &'a TextIndex) -> Result<Self> {
        Ok(Self {
            entity_type,
            index,
            memberships: super::derive::sorted::Scratch::new()?,
            stats: Stats { documents: 0, lengths: vec![0; index.fields.len()] },
        })
    }

    pub(super) fn entity(
        &mut self,
        r: &impl Read,
        seq: u64,
        id: &str,
        mut emit: impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        let doc = document_of(self.index, &field_values(r, self.index, id)?);
        if doc.is_empty() {
            return Ok(());
        }
        let name = self.index.name.as_str();
        let mut membership = |key| {
            // Escaped byte parts keep groups contiguous, including embedded NULs and full-width u64 sequences.
            let order = crate::format::key::encode(&[
                crate::format::key::Part::Bytes(key),
                crate::format::key::Part::Bytes(seq.to_be_bytes().to_vec()),
            ]);
            self.memberships.push(order, Vec::new())
        };
        for (term, places) in &doc.terms {
            membership(keys::text_term(self.entity_type, name, term))?;
            emit(keys::text_positions(self.entity_type, name, term, seq), encode_places(places))?;
        }
        for word in &doc.words {
            membership(keys::text_word(self.entity_type, name, word))?;
        }
        self.stats.documents += 1;
        for (total, length) in self.stats.lengths.iter_mut().zip(&doc.lengths) {
            *total += u64::from(*length);
        }
        emit(keys::text_document(self.entity_type, name, seq), encode_document(id, &doc))
    }

    pub(super) fn finish(self, mut emit: impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>) -> Result<()> {
        use crate::format::key::{Part, decode};
        let mut sorted = self.memberships.finish()?;
        let mut previous: Option<Vec<u8>> = None;
        let mut bitmap = RoaringTreemap::new();
        while let Some((key, _)) = sorted.next()? {
            let mut parts = decode(&key)?.into_iter();
            let (Some(Part::Bytes(key)), Some(Part::Bytes(seq))) = (parts.next(), parts.next()) else {
                return Err(Error::new(ErrorCode::Corrupted, "A sorted text membership doesn't read"));
            };
            let seq = u64::from_be_bytes(
                seq.as_slice()
                    .try_into()
                    .map_err(|_| Error::new(ErrorCode::Corrupted, "A sorted text sequence doesn't read"))?,
            );
            if previous.as_ref().is_some_and(|previous| *previous != key) {
                emit(previous.take().expect("a previous posting exists"), encode_postings(&bitmap))?;
                bitmap = RoaringTreemap::new();
            }
            previous = Some(key);
            bitmap.insert(seq);
        }
        if let Some(key) = previous {
            emit(key, encode_postings(&bitmap))?;
        }
        if self.stats.documents > 0 {
            emit(keys::text_stats(self.entity_type, &self.index.name), encode_stats(&self.stats))?;
        }
        Ok(())
    }
}

/// Deletes every key of a text index of a type
pub fn clear(t: &mut Txn<'_, '_>, entity_type: &str, index: &str) -> Result<()> {
    for range in keys::text_index_ranges(entity_type, index) {
        t.clear_prefix(&range)?;
    }
    t.del(&keys::text_stats(entity_type, index))
}

#[cfg(test)]
mod tests;
