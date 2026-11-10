//! Text indexes (`docs/architecture/search.md`): a type's text fields as terms, kept by every write that changes one, in the same
//! unit. Under a type and an index (`docs/architecture/format.md`, Keys):
//!
//! - `d/ft term` → the places (`seq`) of the entities holding the term, each with the term's count in each field, or
//!   that they are in blocks (`postings`)
//! - `d/fb term start` → a block of those places, for a term more entities hold than its own key keeps
//! - `d/fd seq` → the entity's document: its terms, each with the fields and positions it is at, and its words as
//!   written
//! - `d/fl block` → each field's length in words in the entities of one block of places (`lengths`)
//! - `d/fs` → the index's statistics: how many documents, and each field's total length
//! - `d/fw word` → a stemming index's words as written, each with how many entities hold it: where search as you
//!   type and suggestions find what a word being typed starts, which its stem may not (`runni` starts `running`, held
//!   as `run`)
//!
//! A field holding a list of strings is its elements' terms, each element's positions starting `ELEMENT_GAP` past
//! the last one's, so no phrase spans two elements. A value that is neither is no text, and a term longer than
//! `MAX_TERM_BYTES` isn't held.

pub mod analyze;
pub mod labels;
pub mod lengths;
pub mod postings;
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

/// The text index a read of a type reads, as `resolve` finds it, refused while it is still being built
/// (`INDEX_BUILDING`). A value index's and a trigram index's rows can be walked in its place until it is built; a text
/// index's terms and statistics are the answer themselves, so one read part way through its build would answer from
/// the entities filed so far and say nothing of the rest
pub fn resolve_built(r: &impl Read, entity_type: &str, name: Option<&str>, via: &str) -> Result<TextIndex> {
    let index = resolve(r.schema(), entity_type, name, via)?;
    if super::derive::is_built(r, entity_type, &index.name)? {
        return Ok(index);
    }
    Err(Error::new(
        ErrorCode::IndexBuilding,
        format!(
            "{via}: {entity_type}'s text index \"{}\" is still being built, and would answer only from the entities it \
             has reached: wait for the build (waitForIndexBuilds, on a handle open for writing), then read again",
            index.name
        ),
    ))
}

/// Where an entity holds a term: `(field, positions)` for each field holding it, in field order
pub type Places = Vec<(u32, Vec<u32>)>;

/// One entity as a text index holds it
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Document {
    /// Each field's length in words, in the index's field order
    pub lengths: Vec<u32>,
    /// Each term, with the fields and positions it is at
    pub terms: BTreeMap<String, Places>,
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
    Error::new(ErrorCode::Corrupted, format!("A text index's {what} doesn't read: rebuild() derives it again"))
}

// The stored forms, written and read only here, so `derive` writes exactly what the writes do

/// A set of places as a roaring bitmap, as a trigram index's blocks hold them
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

/// Where an entity holds a term: `[field, positions]` for each field holding it, in field order
fn places_value(places: &[(u32, Vec<u32>)]) -> Value {
    Value::Array(
        places
            .iter()
            .map(|(field, positions)| {
                Value::Array(vec![(*field).into(), Value::Array(positions.iter().map(|p| (*p).into()).collect())])
            })
            .collect(),
    )
}

fn places_from(value: &Value) -> Result<Places> {
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

/// A document's stored record: its terms in term order, each with where the entity holds it, and its words as written
fn encode_document(doc: &Document) -> Vec<u8> {
    values::encode(&Value::Array(vec![
        Value::Array(
            doc.terms
                .iter()
                .map(|(term, places)| Value::Array(vec![term.as_str().into(), places_value(places)]))
                .collect(),
        ),
        Value::Array(doc.words.iter().map(|w| w.as_str().into()).collect()),
    ]))
}

/// A stored document: its terms in term order, each with the fields and positions it is at, and its words as written,
/// in order
#[derive(Debug, Default, PartialEq)]
pub struct StoredDocument {
    pub terms: Vec<(String, Places)>,
    pub words: Vec<String>,
}

impl StoredDocument {
    /// Where the entity holds a term, if it does
    pub fn places(&self, term: &str) -> Option<&[(u32, Vec<u32>)]> {
        let at = self.terms.binary_search_by(|(held, _)| held.as_str().cmp(term)).ok()?;
        Some(&self.terms[at].1)
    }
}

pub fn decode_document(stored: &[u8]) -> Result<StoredDocument> {
    let value = values::decode(stored)?;
    let items = value.as_array().filter(|i| i.len() == 2).ok_or_else(|| corrupt("document"))?;
    let terms = items[0]
        .as_array()
        .ok_or_else(|| corrupt("document"))?
        .iter()
        .map(|item| {
            let pair = item.as_array().filter(|p| p.len() == 2).ok_or_else(|| corrupt("document"))?;
            let term = pair[0].as_str().ok_or_else(|| corrupt("document"))?.to_owned();
            Ok((term, places_from(&pair[1])?))
        })
        .collect::<Result<_>>()?;
    let words = items[1]
        .as_array()
        .ok_or_else(|| corrupt("document"))?
        .iter()
        .map(|t| t.as_str().map(str::to_owned).ok_or_else(|| corrupt("document")))
        .collect::<Result<_>>()?;
    Ok(StoredDocument { terms, words })
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

/// How many entities hold a word as written, as its key holds it
pub fn word_held(stored: &[u8]) -> Result<u64> {
    values::decode(stored)?.as_u64().ok_or_else(|| corrupt("word"))
}

/// Counts one entity more, or one fewer, among those holding a word as written
fn count_word(t: &mut Txn<'_, '_>, key: &[u8], more: bool) -> Result<()> {
    let stored = t.get(key)?;
    let held = stored.as_deref().map(word_held).transpose()?.unwrap_or(0);
    let now = if more { held + 1 } else { held.saturating_sub(1) };
    if now == 0 {
        t.del_over(key, stored.as_deref())
    } else {
        t.put_over(key, &values::encode(&now.into()), stored.as_deref())
    }
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
        file(t, &entity_type, &index, seq, &document_of(&index, &values))?;
    }
    Ok(())
}

/// Files one entity at its place in one text index, from its fields as they are: what a build does for an entity
/// written before the index was declared, and nothing for one a write has already filed
pub fn refile_at(t: &mut Txn<'_, '_>, entity_type: &str, index: &TextIndex, id: &str, seq: u64) -> Result<()> {
    let now = document_of(index, &field_values(t, index, id)?);
    file(t, entity_type, index, seq, &now)
}

/// Takes the entity out of every text index of its type, as destroying it does
pub fn remove(t: &mut Txn<'_, '_>, id: &str) -> Result<()> {
    let entity_type = ids::type_of(id).to_owned();
    let Some(seq) = entities::record(t, id)?.map(|r| r.seq) else { return Ok(()) };
    for index in text_indexes(t.schema, &entity_type) {
        file(t, &entity_type, &index, seq, &Document::default())?;
    }
    Ok(())
}

/// Moves what the index holds of the entity at `seq` from its stored document to `now`
fn file(t: &mut Txn<'_, '_>, entity_type: &str, index: &TextIndex, seq: u64, now: &Document) -> Result<()> {
    let name = index.name.as_str();
    let fields = index.fields.len();
    // Each key read here is replaced with what was read, so the unit's undo log reads none of them again
    let document_key = keys::text_document(entity_type, name, seq);
    let stored_document = t.get(&document_key)?;
    let before = stored_document.as_deref().map(decode_document).transpose()?;
    if before.is_none() && now.is_empty() {
        return Ok(());
    }
    let nothing = StoredDocument::default();
    let held = before.as_ref().unwrap_or(&nothing);
    // A field the index doesn't have is no place a count is kept for
    if held.terms.iter().flat_map(|(_, places)| places).any(|(field, _)| *field as usize >= fields) {
        return Err(corrupt("document"));
    }
    for (term, _) in held.terms.iter().filter(|(term, _)| !now.terms.contains_key(term)) {
        postings::remove(t, &postings::Term::new(entity_type, name, fields, term), seq)?;
    }
    for (term, places) in &now.terms {
        let counts = postings::counts(places, fields);
        // A term the entity holds as often in each field as it did is filed as it was, wherever its positions moved
        if held.places(term).is_some_and(|before| postings::counts(before, fields) == counts) {
            continue;
        }
        postings::put(t, &postings::Term::new(entity_type, name, fields, term), seq, &counts)?;
    }
    for word in held.words.iter().filter(|word| !now.words.contains(*word)) {
        count_word(t, &keys::text_word(entity_type, name, word), false)?;
    }
    for word in now.words.iter().filter(|word| held.words.binary_search(word).is_err()) {
        count_word(t, &keys::text_word(entity_type, name, word), true)?;
    }
    let lengths_key = keys::text_lengths(entity_type, name, lengths::block_of(seq));
    let stored_lengths = t.get(&lengths_key)?;
    let none = vec![0; fields];
    let (held_lengths, block) =
        lengths::set(stored_lengths.as_deref(), seq, if now.is_empty() { &none } else { &now.lengths })?;
    match block {
        Some(block) => t.put_over(&lengths_key, &block, stored_lengths.as_deref())?,
        None => t.del_over(&lengths_key, stored_lengths.as_deref())?,
    }
    let stats_key = keys::text_stats(entity_type, name);
    let stored_stats = t.get(&stats_key)?;
    let mut stats = stats_from(stored_stats.as_deref(), index)?;
    if before.is_some() {
        stats.documents -= 1;
        for (total, l) in stats.lengths.iter_mut().zip(&held_lengths) {
            *total -= u64::from(*l);
        }
    }
    if !now.is_empty() {
        stats.documents += 1;
        for (total, l) in stats.lengths.iter_mut().zip(&now.lengths) {
            *total += u64::from(*l);
        }
        t.put_over(&document_key, &encode_document(now), stored_document.as_deref())?;
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

/// What a row of `Entries` is of, its value's first byte: an entity among a term's, with its counts after it; one
/// among a word as written's; or an entity's lengths, each a little-endian u32, for the block its key is
const TERM: u8 = 0;
const WORD: u8 = 1;
const LENGTHS: u8 = 2;

/// What an index's keys hold, from its entities in any order: each entity's part in a key is a row sorted on disk
/// under the key and its place, so one document and one key's rows are held at a time, a term's no more than a
/// block's worth
pub(super) struct Entries<'a> {
    entity_type: &'a str,
    index: &'a TextIndex,
    rows: super::derive::sorted::Scratch,
    stats: Stats,
}

/// The rows of one key, as they are taken in
enum Rows {
    Term(postings::Filing),
    Word(u64),
    Lengths(Vec<(u64, Vec<u32>)>),
}

impl<'a> Entries<'a> {
    pub(super) fn new(entity_type: &'a str, index: &'a TextIndex) -> Result<Self> {
        Ok(Self {
            entity_type,
            index,
            rows: super::derive::sorted::Scratch::new()?,
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
        let fields = self.index.fields.len();
        let mut row = |key: Vec<u8>, kind: u8, rest: &[u8]| {
            // Escaped byte parts keep a key's rows together, whatever bytes the key and the place hold
            let order = crate::format::key::encode(&[
                crate::format::key::Part::Bytes(key),
                crate::format::key::Part::Bytes(seq.to_be_bytes().to_vec()),
            ]);
            let mut value = Vec::with_capacity(1 + rest.len());
            value.push(kind);
            value.extend_from_slice(rest);
            self.rows.push(order, value)
        };
        for (term, places) in &doc.terms {
            row(keys::text_term(self.entity_type, name, term), TERM, &postings::counts(places, fields))?;
        }
        for word in &doc.words {
            row(keys::text_word(self.entity_type, name, word), WORD, &[])?;
        }
        let lengths: Vec<u8> = doc.lengths.iter().flat_map(|length| length.to_le_bytes()).collect();
        row(keys::text_lengths(self.entity_type, name, lengths::block_of(seq)), LENGTHS, &lengths)?;
        self.stats.documents += 1;
        for (total, length) in self.stats.lengths.iter_mut().zip(&doc.lengths) {
            *total += u64::from(*length);
        }
        emit(keys::text_document(self.entity_type, name, seq), encode_document(&doc))
    }

    pub(super) fn finish(self, mut emit: impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>) -> Result<()> {
        use crate::format::key::{Part, decode};
        let unread = || Error::new(ErrorCode::Corrupted, "A sorted text index row doesn't read");
        let name = self.index.name.as_str();
        let fields = self.index.fields.len();
        let mut sorted = self.rows.finish()?;
        let mut held: Option<(Vec<u8>, Rows)> = None;
        let close = |held: Option<(Vec<u8>, Rows)>, emit: &mut dyn FnMut(Vec<u8>, Vec<u8>) -> Result<()>| match held {
            Some((_, Rows::Term(filing))) => filing.finish(&mut |key, value| emit(key, value)),
            Some((key, Rows::Word(entities))) => emit(key, values::encode(&entities.into())),
            Some((key, Rows::Lengths(places))) => match lengths::block(&places, fields) {
                Some(block) => emit(key, block),
                None => Ok(()),
            },
            None => Ok(()),
        };
        while let Some((order, value)) = sorted.next()? {
            let mut parts = decode(&order)?.into_iter();
            let (Some(Part::Bytes(key)), Some(Part::Bytes(seq))) = (parts.next(), parts.next()) else {
                return Err(unread());
            };
            let seq = u64::from_be_bytes(seq.as_slice().try_into().map_err(|_| unread())?);
            let (&kind, rest) = value.split_first().ok_or_else(unread)?;
            if held.as_ref().is_none_or(|(of, _)| *of != key) {
                close(held.take(), &mut emit)?;
                let rows = match kind {
                    TERM => {
                        let Some(Part::String(term)) = decode(&key)?.pop() else { return Err(unread()) };
                        Rows::Term(postings::Filing::new(postings::Term::new(self.entity_type, name, fields, &term)))
                    }
                    WORD => Rows::Word(0),
                    _ => Rows::Lengths(Vec::new()),
                };
                held = Some((key, rows));
            }
            match held.as_mut().map(|(_, rows)| rows) {
                Some(Rows::Term(filing)) => filing.entry(seq, rest.to_vec(), &mut |key, value| emit(key, value))?,
                Some(Rows::Word(entities)) => *entities += 1,
                Some(Rows::Lengths(places)) => {
                    let lengths = rest.chunks(4).map(|l| l.try_into().map(u32::from_le_bytes).map_err(|_| unread()));
                    places.push((seq, lengths.collect::<Result<_>>()?));
                }
                None => {}
            }
        }
        close(held, &mut emit)?;
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
