//! Trigram indexes (`docs/architecture/search.md`, Substrings and patterns): each run of three characters a type's
//! text fields hold, lowercased, with the places of the entities holding it. Under a type and an index
//! (`docs/architecture/format.md`, Keys):
//!
//! - `d/g trigram block` → the places (`seq`) of the entities holding the trigram, in one block of `2^BLOCK_BITS`
//!   places, as a roaring bitmap. A write changes only its entity's block, and a place only grows, so a new entity's
//!   is the last
//!
//! A substring or a pattern is found by reading the places of the entities holding every trigram a match must hold,
//! then checking each entity's text: the narrowing only ever keeps too many, so what a search answers is what a check
//! of every entity would. A field holding a list of strings is each element apart, so no trigram spans two of them,
//! and a value that is neither holds none.

use std::collections::BTreeSet;

use regex_syntax::hir::{Hir, HirKind};
use rmpv::Value;
use roaring::RoaringTreemap;

use crate::error::{Error, ErrorCode, Result};
use crate::format::key::{Part, decode as decode_key};
use crate::schema::{IndexDef, Schema};

use super::entities;
use super::ids;
use super::keys;
use super::text::{decode_postings, encode_postings};
use super::unit::{Read, Txn};

/// How many places a key's bitmap holds, as a power of two: on G8's data 2^16 holds the index in an eighth of the
/// file's size, where 2^10 takes twice that and a key per entity triples the file (`docs/evidence/next-phase-9.md`)
pub const BLOCK_BITS: u32 = 16;

fn block_of(seq: u64) -> u64 {
    seq >> BLOCK_BITS
}

/// A trigram index on a type, as the writes keep it
#[derive(Debug, Clone, PartialEq)]
pub struct TrigramIndex {
    pub name: String,
    pub fields: Vec<String>,
}

/// The type's trigram indexes
pub fn trigram_indexes(schema: &Schema, entity_type: &str) -> Vec<TrigramIndex> {
    let Some(def) = schema.types.get(entity_type) else { return Vec::new() };
    def.indexes
        .iter()
        .filter_map(|(name, index)| match index {
            IndexDef::Trigram { fields } => Some(TrigramIndex { name: name.clone(), fields: fields.clone() }),
            _ => None,
        })
        .collect()
}

/// The trigram index a step names, or the type's only one; refused (`UNKNOWN_INDEX`) when there is none, or several
/// and none is named
pub fn resolve(schema: &Schema, entity_type: &str, name: Option<&str>, via: &str) -> Result<TrigramIndex> {
    let mut indexes = trigram_indexes(schema, entity_type);
    let found = match name {
        Some(name) => indexes.into_iter().find(|i| i.name == name),
        None if indexes.len() > 1 => {
            return Err(Error::new(
                ErrorCode::UnknownIndex,
                format!("{via}: {entity_type} has several trigram indexes; name the one to read"),
            ));
        }
        None => indexes.pop(),
    };
    found.ok_or_else(|| {
        let which = name.map(|n| format!(" \"{n}\"")).unwrap_or_default();
        Error::new(ErrorCode::UnknownIndex, format!("{via}: {entity_type} has no trigram index{which}"))
    })
}

/// Each text a value holds: a string, or each string of a list
fn texts_of(value: Option<&Value>) -> Vec<&str> {
    match value {
        Some(Value::String(s)) => s.as_str().into_iter().collect(),
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Text as the index holds it, each character lowercased on its own, so the folding of a text holding a substring
/// holds the substring's folding
fn folded(text: &str) -> String {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// Every run of three characters of a text, folded
fn trigrams_in(text: &str, out: &mut BTreeSet<String>) {
    let chars: Vec<char> = folded(text).chars().collect();
    for window in chars.windows(3) {
        out.insert(window.iter().collect());
    }
}

/// The trigrams an entity's index fields hold
fn trigrams_of(values: &[Option<Value>]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for value in values {
        for text in texts_of(value.as_ref()) {
            trigrams_in(text, &mut out);
        }
    }
    out
}

/// The entity's fields an index reads, in its field order, `field` taken as holding `value`
fn values_with(
    r: &impl Read,
    index: &TrigramIndex,
    id: &str,
    field: &str,
    value: Option<&Value>,
) -> Result<Vec<Option<Value>>> {
    index.fields.iter().map(|f| if f == field { Ok(value.cloned()) } else { entities::get(r, id, f) }).collect()
}

/// The entity's fields an index reads, as they are
fn values_of(r: &impl Read, index: &TrigramIndex, id: &str) -> Result<Vec<Option<Value>>> {
    index.fields.iter().map(|f| entities::get(r, id, f)).collect()
}

/// Refiles the entity at `seq` (none for one not stored) in each trigram index of its type covering `field`, which
/// held `before` and now holds `written`: the trigrams it no longer holds lose it, and those it now holds gain it
pub fn refile(
    t: &mut Txn<'_, '_>,
    id: &str,
    field: &str,
    seq: Option<u64>,
    before: Option<&Value>,
    written: Option<&Value>,
) -> Result<()> {
    let Some(seq) = seq else { return Ok(()) };
    let entity_type = ids::type_of(id).to_owned();
    for index in trigram_indexes(t.schema, &entity_type).into_iter().filter(|i| i.fields.iter().any(|f| f == field)) {
        let was = trigrams_of(&values_with(t, &index, id, field, before)?);
        let now = trigrams_of(&values_with(t, &index, id, field, written)?);
        for gone in was.difference(&now) {
            place(t, &keys::trigram(&entity_type, &index.name, gone, block_of(seq)), seq, false)?;
        }
        for added in now.difference(&was) {
            place(t, &keys::trigram(&entity_type, &index.name, added, block_of(seq)), seq, true)?;
        }
    }
    Ok(())
}

/// Adds the place to a block's bitmap, or takes it out, deleting a block left empty. Each key read is replaced with
/// what was read, so the unit's undo log reads none of them again
fn place(t: &mut Txn<'_, '_>, key: &[u8], seq: u64, held: bool) -> Result<()> {
    let stored = t.get(key)?;
    let mut places = stored.as_deref().map(decode_postings).transpose()?.unwrap_or_default();
    let changed = if held { places.insert(seq) } else { places.remove(seq) };
    if !changed {
        return Ok(());
    }
    if places.is_empty() {
        t.del_over(key, stored.as_deref())
    } else {
        t.put_over(key, &encode_postings(&places), stored.as_deref())
    }
}

/// Files one entity at its place in one trigram index from its fields as they are: what a build does for an entity
/// written before the index was declared, and nothing new for one a write has already filed
pub fn refile_at(t: &mut Txn<'_, '_>, entity_type: &str, index: &TrigramIndex, id: &str, seq: u64) -> Result<()> {
    for trigram in trigrams_of(&values_of(t, index, id)?) {
        place(t, &keys::trigram(entity_type, &index.name, &trigram, block_of(seq)), seq, true)?;
    }
    Ok(())
}

/// Takes the entity out of every trigram index of its type, as destroying it does, before its fields go
pub fn remove(t: &mut Txn<'_, '_>, id: &str) -> Result<()> {
    let entity_type = ids::type_of(id).to_owned();
    let Some(seq) = entities::record(t, id)?.map(|r| r.seq) else { return Ok(()) };
    for index in trigram_indexes(t.schema, &entity_type) {
        for trigram in trigrams_of(&values_of(t, &index, id)?) {
            place(t, &keys::trigram(&entity_type, &index.name, &trigram, block_of(seq)), seq, false)?;
        }
    }
    Ok(())
}

/// What a trigram index should hold of the entities at their places
pub fn entries_into(
    r: &impl Read,
    entity_type: &str,
    index: &TrigramIndex,
    of: &[(u64, String)],
    emit: impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>,
) -> Result<()> {
    let mut entries = Entries::new(entity_type, index)?;
    for (seq, id) in of {
        entries.entity(r, *seq, id)?;
    }
    entries.finish(emit)
}

/// Each entity's trigrams kept on disk as they come, so only one block's bitmap is held at a time
pub(super) struct Entries<'a> {
    entity_type: &'a str,
    index: &'a TrigramIndex,
    memberships: super::derive::sorted::Scratch,
}

impl<'a> Entries<'a> {
    pub(super) fn new(entity_type: &'a str, index: &'a TrigramIndex) -> Result<Self> {
        Ok(Self { entity_type, index, memberships: super::derive::sorted::Scratch::new()? })
    }

    pub(super) fn entity(&mut self, r: &impl Read, seq: u64, id: &str) -> Result<()> {
        for trigram in trigrams_of(&values_of(r, self.index, id)?) {
            let key = keys::trigram(self.entity_type, &self.index.name, &trigram, block_of(seq));
            // Escaped byte parts keep a key's places together, in place order
            let order = crate::format::key::encode(&[Part::Bytes(key), Part::Bytes(seq.to_be_bytes().to_vec())]);
            self.memberships.push(order, Vec::new())?;
        }
        Ok(())
    }

    pub(super) fn finish(self, mut emit: impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>) -> Result<()> {
        let corrupt = || Error::new(ErrorCode::Corrupted, "A sorted trigram membership doesn't read");
        let mut sorted = self.memberships.finish()?;
        let mut held: Option<(Vec<u8>, RoaringTreemap)> = None;
        while let Some((order, _)) = sorted.next()? {
            let mut parts = decode_key(&order)?.into_iter();
            let (Some(Part::Bytes(key)), Some(Part::Bytes(seq))) = (parts.next(), parts.next()) else {
                return Err(corrupt());
            };
            let seq = u64::from_be_bytes(seq.as_slice().try_into().map_err(|_| corrupt())?);
            match &mut held {
                Some((previous, places)) if *previous == key => {
                    places.insert(seq);
                }
                _ => {
                    if let Some((previous, places)) = held.take() {
                        emit(previous, encode_postings(&places))?;
                    }
                    held = Some((key, std::iter::once(seq).collect()));
                }
            }
        }
        if let Some((key, places)) = held {
            emit(key, encode_postings(&places))?;
        }
        Ok(())
    }
}

/// Deletes every key of a trigram index of a type
pub fn clear(t: &mut Txn<'_, '_>, entity_type: &str, index: &str) -> Result<()> {
    t.clear_prefix(&keys::trigram_index(entity_type, index))
}

/// What every match's text holds, as far as an index can narrow by it: a text, all of several, one of several, or
/// nothing to narrow by
#[derive(Debug, Clone, PartialEq)]
pub enum Need {
    Any,
    Text(String),
    All(Vec<Need>),
    One(Vec<Need>),
}

/// What a search matches a text against: a substring, exactly or ignoring case, or a pattern
pub enum Matcher {
    Substring { text: String, case_insensitive: bool },
    Pattern(regex::Regex),
}

impl Matcher {
    pub fn substring(text: &str, case_insensitive: bool) -> Matcher {
        Matcher::Substring { text: if case_insensitive { folded(text) } else { text.to_owned() }, case_insensitive }
    }

    /// A pattern in Rust's `regex` syntax, refused (`PATTERN_INVALID`) with the parser's reason when it isn't one:
    /// look-around and backreferences among them, which the syntax doesn't hold
    pub fn pattern(pattern: &str) -> Result<Matcher> {
        regex::Regex::new(pattern)
            .map(Matcher::Pattern)
            .map_err(|e| Error::new(ErrorCode::PatternInvalid, format!("matching \"{pattern}\": {e}")))
    }

    /// What every match's text holds
    pub fn need(&self) -> Need {
        match self {
            Matcher::Substring { text, .. } => Need::Text(folded(text)),
            Matcher::Pattern(regex) => {
                regex_syntax::Parser::new().parse(regex.as_str()).map(|hir| need_of(&hir)).unwrap_or(Need::Any)
            }
        }
    }

    fn is_match(&self, text: &str) -> bool {
        match self {
            Matcher::Substring { text: wanted, case_insensitive: false } => text.contains(wanted.as_str()),
            Matcher::Substring { text: wanted, case_insensitive: true } => folded(text).contains(wanted.as_str()),
            Matcher::Pattern(regex) => regex.is_match(text),
        }
    }

    /// Whether a text of the entity's index fields matches: a string, or one string of a list
    pub fn matches(&self, r: &impl Read, index: &TrigramIndex, id: &str) -> Result<bool> {
        for field in &index.fields {
            if texts_of(entities::get(r, id, field)?.as_ref()).into_iter().any(|text| self.is_match(text)) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// What every match of a pattern holds, read off its syntax: a literal; each part of a sequence; one of an
/// alternation's branches; a repetition's body, when it repeats at least once; a group's body. A class, an anchor or
/// what may match nothing narrows by nothing
fn need_of(hir: &Hir) -> Need {
    match hir.kind() {
        HirKind::Literal(literal) => {
            std::str::from_utf8(&literal.0).map(|t| Need::Text(folded(t))).unwrap_or(Need::Any)
        }
        HirKind::Concat(parts) => Need::All(parts.iter().map(need_of).collect()),
        HirKind::Alternation(branches) => Need::One(branches.iter().map(need_of).collect()),
        HirKind::Repetition(repetition) if repetition.min >= 1 => need_of(&repetition.sub),
        HirKind::Capture(capture) => need_of(&capture.sub),
        _ => Need::Any,
    }
}

/// The places of the entities of a type whose text may hold what `need` asks, or `None` when the index narrows by
/// nothing of it
pub fn candidates(r: &impl Read, entity_type: &str, index: &str, need: &Need) -> Result<Option<RoaringTreemap>> {
    let both = |held: Option<RoaringTreemap>, places: RoaringTreemap| match held {
        Some(held) => held & places,
        None => places,
    };
    Ok(match need {
        Need::Any => None,
        Need::Text(text) => {
            let mut wanted = BTreeSet::new();
            trigrams_in(text, &mut wanted);
            let mut held = None;
            for trigram in &wanted {
                held = Some(both(held, places_of(r, entity_type, index, trigram)?));
                if held.as_ref().is_some_and(RoaringTreemap::is_empty) {
                    break;
                }
            }
            held
        }
        Need::All(parts) => {
            let mut held = None;
            for part in parts {
                if let Some(places) = candidates(r, entity_type, index, part)? {
                    held = Some(both(held, places));
                    if held.as_ref().is_some_and(RoaringTreemap::is_empty) {
                        break;
                    }
                }
            }
            held
        }
        Need::One(branches) => {
            let mut held = RoaringTreemap::new();
            for branch in branches {
                // A branch the index narrows by nothing may match any entity, and so may the alternation
                let Some(places) = candidates(r, entity_type, index, branch)? else { return Ok(None) };
                held |= places;
            }
            Some(held)
        }
    })
}

/// The places of the entities holding one trigram
fn places_of(r: &impl Read, entity_type: &str, index: &str, trigram: &str) -> Result<RoaringTreemap> {
    let mut places = RoaringTreemap::new();
    r.try_visit(&keys::trigram_places(entity_type, index, trigram), |_, block| {
        places |= decode_postings(block)?;
        Ok(())
    })?;
    Ok(places)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn need(pattern: &str) -> Need {
        Matcher::pattern(pattern).unwrap_or_else(|e| panic!("{e:?}")).need()
    }

    fn text(t: &str) -> Need {
        Need::Text(t.into())
    }

    #[test]
    fn a_pattern_needs_what_every_match_holds() {
        assert_eq!(need("error: timeout"), text("error: timeout"));
        assert_eq!(need("Error"), text("error"), "folded, as the index holds it");
        assert_eq!(need("(foo|bar)baz"), Need::All(vec![Need::One(vec![text("foo"), text("bar")]), text("baz")]));
        assert_eq!(need("(?:abc)+x"), Need::All(vec![text("abc"), text("x")]));
        assert_eq!(need("[a-z]+"), Need::Any);
        assert_eq!(need("ab?c"), Need::All(vec![text("a"), Need::Any, text("c")]));
    }

    #[test]
    fn look_around_and_backreferences_are_refused_naming_the_pattern() {
        for (pattern, reason) in
            [("foo(?=bar)", "look-around"), ("(?<!a)b", "look-around"), (r"(a)\1", "backreferences"), ("open(", "")]
        {
            let Err(error) = Matcher::pattern(pattern) else { panic!("{pattern} was taken") };
            assert_eq!(error.code, ErrorCode::PatternInvalid, "{pattern}");
            assert!(error.message.contains(pattern) && error.message.contains(reason), "{}", error.message);
        }
    }

    #[test]
    fn a_list_s_elements_are_apart_and_case_is_folded() {
        let held =
            trigrams_of(&[Some(Value::Array(vec!["AB".into(), "CD".into()])), Some("Xyz".into()), Some(3.into())]);
        assert_eq!(held.into_iter().collect::<Vec<_>>(), vec!["xyz"]);
    }
}
