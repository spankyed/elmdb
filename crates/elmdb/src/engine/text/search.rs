//! Searching a text index (`docs/architecture/search.md`, Methods): a query's clauses, each clause's words expanded to the index's
//! terms — the word itself, the terms it starts for search as you type (in a stemming index, the stems of the words as
//! written it starts), and those an edit or two away — matched
//! through the posting lists, checked against positions where a phrase or a field asks it, and ranked by BM25F, each
//! field's term frequency and length counted at its weight.
//!
//! The query: words, all of which match (or any, with `any`); `"a phrase"`, its words adjacent in one field;
//! `-word` or `-"a phrase"`, which no match holds; `field:word`, held in that field of the index; and `a OR b`, either.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use levenshtein_automata::{DFA, Distance, LevenshteinAutomatonBuilder, SINK_STATE};
use roaring::RoaringTreemap;

use crate::error::{Error, ErrorCode, Result};
use crate::format::key::{Part, decode as decode_key};

use super::super::keys;
use super::super::unit::Read;
use super::{TextIndex, decode_places, decode_postings, document, stats, texts_at};
use crate::engine::plan::SearchOptions;

/// BM25's saturation of term frequency, and its normalization by length
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// What a matching term scores beside the word itself: a term the word starts, and one or two edits away
const PREFIX_WEIGHT: f64 = 0.8;
const ONE_EDIT_WEIGHT: f64 = 0.6;
const TWO_EDITS_WEIGHT: f64 = 0.4;

/// The most terms a word expands to by prefix, the most held kept, so one letter typed doesn't match the dictionary
const MAX_PREFIX_TERMS: usize = 64;

/// One clause of a query as typed, before an index's analyzer reads it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawClause {
    pub text: String,
    pub quoted: bool,
    pub negated: bool,
    pub field: Option<String>,
    /// Joined to the clause before it by `OR`
    pub or_previous: bool,
}

/// Splits a query into its clauses. A `name:` is a field only where the index has a field of that name, which
/// `clauses` decides; here it is kept apart from the text after it
pub fn parse(query: &str) -> Vec<RawClause> {
    let chars: Vec<char> = query.chars().collect();
    let mut out: Vec<RawClause> = Vec::new();
    let mut at = 0;
    let mut or_next = false;
    while at < chars.len() {
        if chars[at].is_whitespace() {
            at += 1;
            continue;
        }
        let negated = chars[at] == '-' && chars.get(at + 1).is_some_and(|c| !c.is_whitespace());
        if negated {
            at += 1;
        }
        let mut field = None;
        // A field name, then a colon, then what is held in it
        let name_end = (at..chars.len()).find(|&i| !chars[i].is_ascii_alphanumeric()).unwrap_or(chars.len());
        if name_end > at
            && chars[at].is_ascii_alphabetic()
            && chars.get(name_end) == Some(&':')
            && chars.get(name_end + 1).is_some_and(|c| !c.is_whitespace())
        {
            field = Some(chars[at..name_end].iter().collect::<String>());
            at = name_end + 1;
        }
        let (text, quoted) = if chars[at] == '"' {
            let end = (at + 1..chars.len()).find(|&i| chars[i] == '"').unwrap_or(chars.len());
            let text: String = chars[at + 1..end].iter().collect();
            at = (end + 1).min(chars.len());
            (text, true)
        } else {
            let end = (at..chars.len()).find(|&i| chars[i].is_whitespace()).unwrap_or(chars.len());
            let text: String = chars[at..end].iter().collect();
            at = end;
            (text, false)
        };
        if !quoted && !negated && field.is_none() && text == "OR" && !out.is_empty() {
            or_next = true;
            continue;
        }
        out.push(RawClause { text, quoted, negated, field, or_previous: std::mem::take(&mut or_next) });
    }
    out
}

/// A word of a clause: the terms the analyzer read at one place of it, any of which matches
#[derive(Debug, Clone)]
struct Word {
    alternatives: Vec<String>,
    /// The words as written those terms were read from, which a prefix starts unstemmed
    written: Vec<String>,
    /// The clause's last word, matched as a prefix too
    prefix: bool,
}

/// A clause as an index reads it
#[derive(Debug, Clone)]
struct Clause {
    words: Vec<Word>,
    /// The index's fields it may be held in
    fields: Option<Vec<u32>>,
    negated: bool,
    /// Clauses of one group are joined by `OR`
    group: usize,
}

/// The query's clauses under an index's analyzer and fields; a clause the analyzer reads no word in is dropped
fn clauses(index: &TextIndex, raw: &[RawClause], options: &SearchOptions) -> Vec<Clause> {
    let field_of = |name: &str| index.fields.iter().position(|(f, _)| f == name).map(|i| i as u32);
    let allowed: Option<Vec<u32>> =
        options.fields.as_ref().map(|names| names.iter().filter_map(|n| field_of(n)).collect());
    let mut out = Vec::new();
    let mut group = 0;
    let last = raw.len().saturating_sub(1);
    for (at, clause) in raw.iter().enumerate() {
        // A name the index has no field of is part of the text: `http://…` is no field
        let (fields, text) = match clause.field.as_deref() {
            Some(name) => match field_of(name) {
                Some(f) => (Some(vec![f]), clause.text.clone()),
                None => (allowed.clone(), format!("{name}:{}", clause.text)),
            },
            None => (allowed.clone(), clause.text.clone()),
        };
        let mut by_place: BTreeMap<u32, (Vec<String>, Vec<String>)> = BTreeMap::new();
        for term in index.analyzer.terms(&text) {
            let (alternatives, written) = by_place.entry(term.position).or_default();
            if !alternatives.contains(&term.text) {
                alternatives.push(term.text);
            }
            if !written.contains(&term.word) {
                written.push(term.word);
            }
        }
        if by_place.is_empty() {
            continue;
        }
        let prefix = options.prefix && at == last && !clause.quoted && !clause.negated;
        let count = by_place.len();
        let words = by_place
            .into_values()
            .enumerate()
            .map(|(i, (alternatives, written))| Word { alternatives, written, prefix: prefix && i + 1 == count })
            .collect();
        if !clause.or_previous || out.is_empty() {
            group += 1;
        }
        out.push(Clause { words, fields, negated: clause.negated, group });
    }
    out
}

/// The automata for one and two edits, built once: a builder costs more than many searches
fn automaton(distance: u8) -> &'static LevenshteinAutomatonBuilder {
    static ONE: OnceLock<LevenshteinAutomatonBuilder> = OnceLock::new();
    static TWO: OnceLock<LevenshteinAutomatonBuilder> = OnceLock::new();
    if distance == 1 {
        ONE.get_or_init(|| LevenshteinAutomatonBuilder::new(1, true))
    } else {
        TWO.get_or_init(|| LevenshteinAutomatonBuilder::new(2, true))
    }
}

/// The edits a word of this length may be away from a term and still match it
fn edits_for(word: &str) -> u8 {
    match word.chars().count() {
        0..=4 => 0,
        5..=8 => 1,
        _ => 2,
    }
}

/// The term a key of a text index's dictionary holds
fn term_of(key: &[u8]) -> Result<String> {
    match decode_key(key)?.pop() {
        Some(Part::String(term)) => Ok(term),
        _ => Err(Error::new(ErrorCode::Corrupted, "A text index's term key doesn't read")),
    }
}

/// Every entry of a dictionary of the index (its terms, or its words as written) within `distance` edits of `word`,
/// with its distance and its posting list. The automaton reads each entry a byte at a time, and where no entry starting
/// with the bytes read so far can match, the walk seeks past all of them: a dictionary is read where it can match, not
/// whole. An entry's key is the dictionary's prefix, a string's tag and the entry's bytes, ended by a zero; an entry
/// holding a zero, which an analyzer never makes, is read whole
fn within(r: &impl Read, dictionary: &[u8], dfa: &DFA, distance: u8) -> Result<Vec<(String, u8, RoaringTreemap)>> {
    // The entry's bytes start after the dictionary's prefix and the string's tag
    let head = dictionary.len() + 1;
    let mut out = Vec::new();
    let mut from = dictionary.to_vec();
    loop {
        let mut failed = None;
        let mut seek: Option<Vec<u8>> = None;
        r.visit_from(dictionary, &from, |key, value| {
            let bytes = key.get(head..key.len().saturating_sub(1)).unwrap_or_default();
            let state = if bytes.contains(&0) {
                None
            } else {
                let mut state = dfa.initial_state();
                for (at, &byte) in bytes.iter().enumerate() {
                    state = dfa.transition(state, byte);
                    if state == SINK_STATE {
                        // No entry starting with these bytes matches: on to the first that doesn't start with them. A
                        // UTF-8 byte is never 0xFF, so raising the last one is that entry's lower bound
                        let mut past = key[..head + at + 1].to_vec();
                        *past.last_mut().expect("a byte was read") += 1;
                        seek = Some(past);
                        return false;
                    }
                }
                Some(state)
            };
            let found = term_of(key).and_then(|entry| {
                let distance_of = match state {
                    Some(state) => dfa.distance(state),
                    None => dfa.eval(&entry),
                };
                match distance_of {
                    Distance::Exact(d) if d <= distance => Ok(Some((entry, d, decode_postings(value)?))),
                    _ => Ok(None),
                }
            });
            match found {
                Ok(Some(entry)) => out.push(entry),
                Ok(None) => {}
                Err(e) => {
                    failed = Some(e);
                    return false;
                }
            }
            true
        })?;
        if let Some(e) = failed {
            return Err(e);
        }
        match seek {
            Some(past) => from = past,
            None => return Ok(out),
        }
    }
}

/// The terms of the index a word matches, each with what a match of it scores beside the word itself and its posting
/// list
type Expansions = BTreeMap<String, (f64, RoaringTreemap)>;

/// Adds a term to a word's expansions at a weight, keeping the higher where it is there already
fn merge(
    out: &mut Expansions,
    term: String,
    weight: f64,
    postings: impl FnOnce() -> Result<Option<RoaringTreemap>>,
) -> Result<()> {
    if let Some((held, _)) = out.get_mut(&term) {
        *held = held.max(weight);
        return Ok(());
    }
    if let Some(p) = postings()? {
        out.insert(term, (weight, p));
    }
    Ok(())
}

/// The entries of a dictionary of the index starting with the bytes `starting`, each with its posting list, most held
/// first
fn started(r: &impl Read, starting: &[u8]) -> Result<Vec<(String, RoaringTreemap)>> {
    let mut out: Vec<(String, RoaringTreemap)> = Vec::new();
    let mut failed = None;
    r.visit_starting(starting, |key, value| {
        match (term_of(key), decode_postings(value)) {
            (Ok(entry), Ok(p)) => out.push((entry, p)),
            (Err(e), _) | (_, Err(e)) => {
                failed = Some(e);
                return false;
            }
        }
        true
    })?;
    if let Some(e) = failed {
        return Err(e);
    }
    out.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

fn expand(r: &impl Read, entity_type: &str, index: &TextIndex, word: &Word, fuzzy: bool) -> Result<Expansions> {
    let name = index.name.as_str();
    let mut out = Expansions::new();
    let postings = |term: &str| -> Result<Option<RoaringTreemap>> {
        r.get(&keys::text_term(entity_type, name, term))?.map(|b| decode_postings(&b)).transpose()
    };
    for alternative in &word.alternatives {
        merge(&mut out, alternative.clone(), 1.0, || postings(alternative))?;
        if word.prefix {
            for (term, p) in started(r, &keys::text_terms_starting(entity_type, name, alternative))?
                .into_iter()
                .take(MAX_PREFIX_TERMS)
            {
                merge(&mut out, term, PREFIX_WEIGHT, || Ok(Some(p)))?;
            }
        }
        let edits = if fuzzy { edits_for(alternative) } else { 0 };
        if edits > 0 {
            let dfa = automaton(edits).build_dfa(alternative);
            for (term, d, p) in within(r, &keys::text_terms(entity_type, name), &dfa, edits)? {
                if d > 0 {
                    let weight = if d == 1 { ONE_EDIT_WEIGHT } else { TWO_EDITS_WEIGHT };
                    merge(&mut out, term, weight, || Ok(Some(p)))?;
                }
            }
        }
    }
    // A stem need not start as its word does (`runni` starts `running`, held as `run`): in a stemming index the prefix
    // is matched against the words as written, each standing for the term it is held as
    if word.prefix && index.analyzer.stems() {
        for written in &word.written {
            let found = started(r, &keys::text_words_starting(entity_type, name, written))?;
            for (held, _) in found.into_iter().take(MAX_PREFIX_TERMS) {
                let term = index.analyzer.stem(&held);
                merge(&mut out, term.clone(), PREFIX_WEIGHT, || postings(&term))?;
            }
        }
    }
    Ok(out)
}

/// What a search found in one entity
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: String,
    pub seq: u64,
    pub score: f64,
    /// The index's terms it matched, in term order
    pub terms: Vec<String>,
    /// The fields it was matched in, which its highlights are of
    pub fields: Vec<u32>,
}

/// A clause's words expanded, and the entities holding every word
struct Matched {
    words: Vec<Expansions>,
    candidates: RoaringTreemap,
}

fn union(postings: &Expansions) -> RoaringTreemap {
    postings.values().fold(RoaringTreemap::new(), |acc, (_, p)| acc | p)
}

/// Each place of the entity's fields a word's terms are at, by field: `field → positions`
fn places_of(
    r: &impl Read,
    entity_type: &str,
    index: &str,
    word: &Expansions,
    seq: u64,
) -> Result<BTreeMap<u32, HashSet<u32>>> {
    let mut out: BTreeMap<u32, HashSet<u32>> = BTreeMap::new();
    for (term, (_, postings)) in word {
        if !postings.contains(seq) {
            continue;
        }
        if let Some(stored) = r.get(&keys::text_positions(entity_type, index, term, seq))? {
            for (field, positions) in decode_places(&stored)? {
                out.entry(field).or_default().extend(positions);
            }
        }
    }
    Ok(out)
}

/// Whether the entity holds the clause: each word in one of its fields, a phrase's words at consecutive places
fn holds(r: &impl Read, entity_type: &str, index: &str, clause: &Clause, matched: &Matched, seq: u64) -> Result<bool> {
    let in_field = |f: &u32| clause.fields.as_ref().is_none_or(|fs| fs.contains(f));
    if matched.words.len() == 1 && clause.fields.is_none() {
        return Ok(true);
    }
    let mut places = Vec::with_capacity(matched.words.len());
    for word in &matched.words {
        places.push(places_of(r, entity_type, index, word, seq)?);
    }
    for (field, first) in places[0].iter().filter(|(f, _)| in_field(f)) {
        let starts = first.iter().filter(|&&p| {
            places
                .iter()
                .enumerate()
                .skip(1)
                .all(|(i, word)| word.get(field).is_some_and(|ps| ps.contains(&(p + i as u32))))
        });
        if starts.count() > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The entities of a type the query matches in one of its text indexes, within `among` when given, each with its
/// score and the terms it matched. `now` measures a recency option's ages
pub fn search(
    r: &impl Read,
    entity_type: &str,
    index: &TextIndex,
    query: &[RawClause],
    options: &SearchOptions,
    among: Option<&RoaringTreemap>,
    now: f64,
) -> Result<Vec<Hit>> {
    let name = index.name.as_str();
    let clauses = clauses(index, query, options);
    let mut matched = Vec::with_capacity(clauses.len());
    for clause in &clauses {
        let words: Vec<_> =
            clause.words.iter().map(|w| expand(r, entity_type, index, w, options.fuzzy)).collect::<Result<_>>()?;
        let mut candidates = words.first().map(union).unwrap_or_default();
        for word in words.iter().skip(1) {
            candidates &= union(word);
        }
        if let Some(among) = among {
            candidates &= among;
        }
        let needs_places = words.len() > 1 || clause.fields.is_some();
        if needs_places {
            let mut kept = RoaringTreemap::new();
            let probe = Matched { words, candidates: RoaringTreemap::new() };
            for seq in &candidates {
                if holds(r, entity_type, name, clause, &probe, seq)? {
                    kept.insert(seq);
                }
            }
            matched.push(Matched { words: probe.words, candidates: kept });
        } else {
            matched.push(Matched { words, candidates });
        }
    }
    // The groups' matches, joined: every group, or with `any` one of them, and no negated clause
    let mut groups: BTreeMap<usize, RoaringTreemap> = BTreeMap::new();
    let mut excluded = RoaringTreemap::new();
    for (clause, m) in clauses.iter().zip(&matched) {
        if clause.negated {
            excluded |= &m.candidates;
        } else {
            *groups.entry(clause.group).or_default() |= &m.candidates;
        }
    }
    let mut found: Option<RoaringTreemap> = None;
    for g in groups.into_values() {
        found = Some(match found {
            None => g,
            Some(f) if options.any => f | g,
            Some(f) => f & g,
        });
    }
    let found = found.unwrap_or_default() - excluded;
    if found.is_empty() {
        return Ok(Vec::new());
    }
    let stats = stats(r, entity_type, index)?;
    let n = stats.documents.max(1) as f64;
    let allowed: Vec<u32> = match &options.fields {
        Some(names) => {
            names.iter().filter_map(|f| index.fields.iter().position(|(n, _)| n == f).map(|i| i as u32)).collect()
        }
        None => (0..index.fields.len() as u32).collect(),
    };
    let weight = |f: u32| index.fields.get(f as usize).map(|(_, w)| *w).unwrap_or(0.0);
    let average: f64 = allowed.iter().map(|&f| weight(f) * stats.lengths[f as usize] as f64 / n).sum::<f64>().max(1.0);
    let mut hits = Vec::with_capacity(found.len() as usize);
    for seq in &found {
        let Some(doc) = document(r, entity_type, name, seq)? else { continue };
        let length: f64 = allowed.iter().map(|&f| weight(f) * f64::from(doc.lengths[f as usize])).sum();
        let mut score = 0.0;
        let mut terms: HashSet<String> = HashSet::new();
        let mut fields: HashSet<u32> = HashSet::new();
        for (clause, m) in clauses.iter().zip(&matched) {
            if clause.negated || !m.candidates.contains(seq) {
                continue;
            }
            let in_clause: Vec<u32> = clause.fields.clone().unwrap_or_else(|| allowed.clone());
            for word in &m.words {
                let mut best: f64 = 0.0;
                for (term, (w, postings)) in word {
                    if !postings.contains(seq) {
                        continue;
                    }
                    let Some(stored) = r.get(&keys::text_positions(entity_type, name, term, seq))? else { continue };
                    let mut frequency = 0.0;
                    for (field, positions) in decode_places(&stored)? {
                        if in_clause.contains(&field) {
                            frequency += weight(field) * positions.len() as f64;
                            fields.insert(field);
                        }
                    }
                    if frequency == 0.0 {
                        continue;
                    }
                    terms.insert(term.clone());
                    let df = postings.len() as f64;
                    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                    let tf = frequency * (K1 + 1.0) / (frequency + K1 * (1.0 - B + B * length / average));
                    best = best.max(w * idf * tf);
                }
                score += best;
            }
        }
        if let Some(recency) = &options.recency
            && recency.half_life > 0.0
            && let Some(at) = super::super::entities::get(r, &doc.id, &recency.field)?
                .as_ref()
                .and_then(super::super::values::as_time)
        {
            score *= 0.5f64.powf(((now - at).max(0.0)) / recency.half_life);
        }
        let mut terms: Vec<String> = terms.into_iter().collect();
        terms.sort();
        let mut fields: Vec<u32> = fields.into_iter().collect();
        fields.sort();
        hits.push(Hit { id: doc.id, seq, score, terms, fields });
    }
    Ok(hits)
}

/// Where a hit's terms are in each of its fields: `[element, start, end]` in UTF-16 units, overlapping marks joined
pub fn highlights(r: &impl Read, index: &TextIndex, hit: &Hit) -> Result<Vec<(String, Vec<[u32; 3]>)>> {
    let terms: HashSet<&str> = hit.terms.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for &f in &hit.fields {
        let Some((name, _)) = index.fields.get(f as usize) else { continue };
        let value = super::super::entities::get(r, &hit.id, name)?;
        let mut marks = Vec::new();
        for (element, text) in texts_at(value.as_ref()) {
            let mut spans: Vec<(usize, usize)> = index
                .analyzer
                .terms(text)
                .into_iter()
                .filter(|t| terms.contains(t.text.as_str()))
                .map(|t| (t.start, t.end))
                .collect();
            spans.sort();
            let mut joined: Vec<(usize, usize)> = Vec::new();
            for (start, end) in spans {
                match joined.last_mut() {
                    Some(last) if start <= last.1 => last.1 = last.1.max(end),
                    _ => joined.push((start, end)),
                }
            }
            for (start, end) in joined {
                let units = |byte: usize| text[..byte].encode_utf16().count() as u32;
                marks.push([element as u32, units(start), units(end)]);
            }
        }
        if !marks.is_empty() {
            out.push((name.clone(), marks));
        }
    }
    Ok(out)
}

/// The words a word being typed starts, most held first, and the words nearest it, nearest first: what `suggest`
/// answers. A stemming index answers from its words as written, so a suggestion is a word and not a stem
pub fn suggest(
    r: &impl Read,
    entity_type: &str,
    index: &TextIndex,
    word: &str,
    limit: usize,
) -> Result<(Vec<String>, Vec<String>)> {
    let name = index.name.as_str();
    let Some(first) = index.analyzer.terms(word).into_iter().next() else {
        return Ok((Vec::new(), Vec::new()));
    };
    // A stemming index's words as written, or the terms of one whose terms are its words
    let (term, dictionary, starting) = if index.analyzer.stems() {
        let starting = keys::text_words_starting(entity_type, name, &first.word);
        (first.word, keys::text_words(entity_type, name), starting)
    } else {
        let starting = keys::text_terms_starting(entity_type, name, &first.text);
        (first.text, keys::text_terms(entity_type, name), starting)
    };
    let completions = started(r, &starting)?.into_iter().take(limit).map(|(t, _)| t).collect();
    let edits = match term.chars().count() {
        0..=2 => 0,
        3..=6 => 1,
        _ => 2,
    };
    let mut near: Vec<(u8, u64, String)> = Vec::new();
    if edits > 0 {
        let dfa = automaton(edits).build_dfa(&term);
        for (t, d, p) in within(r, &dictionary, &dfa, edits)? {
            if d > 0 {
                near.push((d, p.len(), t));
            }
        }
    }
    near.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)).then_with(|| a.2.cmp(&b.2)));
    let corrections = near.into_iter().take(limit).map(|(_, _, t)| t).collect();
    Ok((completions, corrections))
}

/// The ids' places among the index's documents, for a search within them
pub fn places(r: &impl Read, ids: &[String]) -> Result<HashMap<u64, String>> {
    let mut out = HashMap::with_capacity(ids.len());
    for id in ids {
        if let Some(record) = super::super::entities::record(r, id)? {
            out.insert(record.seq, id.clone());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod seeking {
    //! The fuzzy walk against the dictionary read whole: what it finds, and how little of it it reads

    use super::*;
    use crate::schema::Schema;
    use crate::store::Db;
    use crate::{Database, OpenOptions};
    use serde_json::json;
    use std::cell::Cell;

    /// A read that counts the entries it hands over
    struct Counting<'a, R> {
        inner: &'a R,
        entries: Cell<usize>,
    }
    impl<R: Read> Read for Counting<'_, R> {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            self.inner.get(key)
        }
        fn visit(&self, prefix: &[u8], mut f: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
            self.inner.visit(prefix, |k, v| {
                self.entries.set(self.entries.get() + 1);
                f(k, v)
            })
        }
        fn visit_starting(&self, prefix: &[u8], mut f: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
            self.inner.visit_starting(prefix, |k, v| {
                self.entries.set(self.entries.get() + 1);
                f(k, v)
            })
        }
        fn visit_from(&self, prefix: &[u8], from: &[u8], mut f: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
            self.inner.visit_from(prefix, from, |k, v| {
                self.entries.set(self.entries.get() + 1);
                f(k, v)
            })
        }
        fn schema(&self) -> &Schema {
            self.inner.schema()
        }
        fn raw(&self) -> (&heed::RoTxn<'_, heed::AnyTls>, Db) {
            self.inner.raw()
        }
    }

    /// A word of five to eleven letters from twenty, so some lie an edit or two from each other and most don't
    fn word(mut n: u64) -> String {
        let mut out = String::new();
        let len = 5 + (n % 7) as usize;
        for _ in 0..len {
            out.push(char::from(b'a' + (n % 20) as u8));
            n = n.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407) >> 7;
        }
        out
    }

    #[test]
    fn a_fuzzy_walk_finds_what_reading_every_term_finds_and_reads_far_fewer() {
        let dir = tempfile::tempdir().unwrap();
        let schema = Schema::from_json(&json!({
            "$schema": crate::schema::SCHEMA_LANGUAGE,
            "types": { "Message": {
                "fields": { "text": { "type": "string", "optional": true } },
                "indexes": { "text": { "kind": "text", "fields": { "text": {} } } }
            } }
        }))
        .unwrap();
        let db = Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() })
            .unwrap();
        // Words, and names a chat about code holds — ids that share their first letters, as a dictionary's many do
        let queries: Vec<String> = (0..200u64).map(|n| word(n * 31 + 7)).collect();
        let mut words: Vec<String> = (0..3_000).map(word).chain((0..20_000u64).map(|n| format!("idk{n:05x}"))).collect();
        // Beside each query, a term the walk gives up on at its third byte and, one byte value above it there, a term an
        // edit away: a seek past more than the dead term's own prefix would skip the match
        for query in &queries {
            let mut close = query.clone().into_bytes();
            close[0] = if close[0] == b'z' { b'y' } else { b'z' };
            let mut dead = close.clone();
            if dead[2] > b'a' {
                dead[2] -= 1;
                words.push(String::from_utf8(dead).unwrap());
                words.push(String::from_utf8(close).unwrap());
            }
        }
        let ops: Vec<_> = words
            .chunks(10)
            .enumerate()
            .map(|(i, chunk)| json!({ "op": "create", "type": "Message", "id": format!("Message-{i}"), "fields": { "text": chunk.join(" ") } }))
            .collect();
        crate::engine::tests::write(&db, ops).unwrap();
        let dictionary = keys::text_terms("Message", "text");
        db.snapshot(|r| {
            let mut whole = Vec::new();
            r.visit(&dictionary, |key, _| {
                whole.push(term_of(key).unwrap());
                true
            })?;
            assert!(whole.len() > 22_000, "the fixture's dictionary: {}", whole.len());
            let mut read = 0;
            for (query, edits) in queries.iter().zip((0..).map(|n: u64| 1 + (n % 2) as u8)) {
                let dfa = automaton(edits).build_dfa(query);
                let expected: Vec<(String, u8)> = whole
                    .iter()
                    .filter_map(|term| match dfa.eval(term) {
                        Distance::Exact(d) if d <= edits => Some((term.clone(), d)),
                        _ => None,
                    })
                    .collect();
                let counting = Counting { inner: r, entries: Cell::new(0) };
                let found: Vec<(String, u8)> =
                    within(&counting, &dictionary, &dfa, edits)?.into_iter().map(|(term, d, _)| (term, d)).collect();
                assert_eq!(found, expected, "{query} within {edits}");
                read += counting.entries.get();
            }
            // The walk reads an entry where a match could start: a small part of every query reading the whole dictionary
            assert!(read * 10 < whole.len() * 200, "read {read} entries for 200 walks of {}", whole.len());
            Ok(())
        })
        .unwrap();
    }
}
