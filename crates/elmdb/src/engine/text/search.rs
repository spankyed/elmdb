//! Searching a text index (`docs/architecture/search.md`, Methods): a query's clauses, each clause's words expanded to the index's
//! terms — the word itself, the terms it starts for search as you type, and those an edit or two away — matched
//! through the posting lists, checked against positions where a phrase or a field asks it, and ranked by BM25F, each
//! field's term frequency and length counted at its weight.
//!
//! The query: words, all of which match (or any, with `any`); `"a phrase"`, its words adjacent in one field;
//! `-word` or `-"a phrase"`, which no match holds; `field:word`, held in that field of the index; and `a OR b`, either.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use levenshtein_automata::{DFA, Distance, LevenshteinAutomatonBuilder};
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
        let mut by_place: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for term in index.analyzer.terms(&text) {
            let alternatives = by_place.entry(term.position).or_default();
            if !alternatives.contains(&term.text) {
                alternatives.push(term.text);
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
            .map(|(i, alternatives)| Word { alternatives, prefix: prefix && i + 1 == count })
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

/// Every term of the index within `distance` edits of `word`, with its distance
fn within(r: &impl Read, entity_type: &str, index: &str, dfa: &DFA, distance: u8) -> Result<Vec<(String, u8)>> {
    let mut out = Vec::new();
    let mut failed = None;
    r.visit(&keys::text_terms(entity_type, index), |key, _| {
        match term_of(key) {
            Ok(term) => {
                if let Distance::Exact(d) = dfa.eval(&term)
                    && d <= distance
                {
                    out.push((term, d));
                }
            }
            Err(e) => {
                failed = Some(e);
                return false;
            }
        }
        true
    })?;
    match failed {
        Some(e) => Err(e),
        None => Ok(out),
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

fn expand(r: &impl Read, entity_type: &str, index: &str, word: &Word, fuzzy: bool) -> Result<Expansions> {
    let mut out = Expansions::new();
    let postings = |term: &str| -> Result<Option<RoaringTreemap>> {
        r.get(&keys::text_term(entity_type, index, term))?.map(|b| decode_postings(&b)).transpose()
    };
    for alternative in &word.alternatives {
        merge(&mut out, alternative.clone(), 1.0, || postings(alternative))?;
        if word.prefix {
            let mut started: Vec<(String, RoaringTreemap)> = Vec::new();
            let mut failed = None;
            r.visit_starting(&keys::text_terms_starting(entity_type, index, alternative), |key, value| {
                match (term_of(key), decode_postings(value)) {
                    (Ok(term), Ok(p)) => started.push((term, p)),
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
            started.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
            for (term, p) in started.into_iter().take(MAX_PREFIX_TERMS) {
                merge(&mut out, term, PREFIX_WEIGHT, || Ok(Some(p)))?;
            }
        }
        let edits = if fuzzy { edits_for(alternative) } else { 0 };
        if edits > 0 {
            let dfa = automaton(edits).build_dfa(alternative);
            for (term, d) in within(r, entity_type, index, &dfa, edits)? {
                if d > 0 {
                    let weight = if d == 1 { ONE_EDIT_WEIGHT } else { TWO_EDITS_WEIGHT };
                    merge(&mut out, term.clone(), weight, || postings(&term))?;
                }
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
            clause.words.iter().map(|w| expand(r, entity_type, name, w, options.fuzzy)).collect::<Result<_>>()?;
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

/// The terms a word being typed starts, most held first, and the terms nearest it, nearest first: what `suggest`
/// answers
pub fn suggest(
    r: &impl Read,
    entity_type: &str,
    index: &TextIndex,
    word: &str,
    limit: usize,
) -> Result<(Vec<String>, Vec<String>)> {
    let name = index.name.as_str();
    let Some(term) = index.analyzer.terms(word).into_iter().next().map(|t| t.text) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut started: Vec<(String, u64)> = Vec::new();
    let mut failed = None;
    r.visit_starting(&keys::text_terms_starting(entity_type, name, &term), |key, value| {
        match (term_of(key), decode_postings(value)) {
            (Ok(t), Ok(p)) => started.push((t, p.len())),
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
    started.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let completions = started.into_iter().take(limit).map(|(t, _)| t).collect();
    let edits = match term.chars().count() {
        0..=2 => 0,
        3..=6 => 1,
        _ => 2,
    };
    let mut near: Vec<(u8, u64, String)> = Vec::new();
    if edits > 0 {
        let dfa = automaton(edits).build_dfa(&term);
        for (t, d) in within(r, entity_type, name, &dfa, edits)? {
            if d == 0 {
                continue;
            }
            let held = r
                .get(&keys::text_term(entity_type, name, &t))?
                .map(|b| decode_postings(&b))
                .transpose()?
                .map(|p| p.len())
                .unwrap_or(0);
            near.push((d, held, t));
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
