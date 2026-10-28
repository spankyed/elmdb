//! Running reads (`plan.rs`) against a snapshot or a unit's transaction, and writing their answers as one msgpack
//! envelope. A query pulls ids through its steps one at a time, so `first`, `exists`, `count` and `limit` stop once they
//! have their answer; a step needing the whole set (`orderBy`, `reverse`, `distinct`, the deep walks) collects it.
//! A type start's leading filters are read from a value index where one covers a `where` or `withRole` among them, its
//! matches visited as they are read; `orderBy` followed by `limit` holds only the first `limit`.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use rmpv::Value;

use crate::error::{Error, ErrorCode, Result};
use crate::format::key::{Part, decode as decode_key};
use crate::format::value::TIMESTAMP_EXTENSION;

use super::entities::{self, field};
use super::keys::{self, ROLE_INDEX, SHORT_CODE_INDEX};
use super::plan::{Plan, ReadOp, Scope, SearchOptions, Start, Step, Terminal};
use super::text::{self, search::Hit};
use super::trigram::{self, Matcher};
use super::unit::Read;
use super::{ids, relations, values};

/// What one plan's run read, for the specs that show a query stops early
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Visited {
    pub entities: u64,
}

/// Runs reads, writing each answer into one msgpack array
pub fn run(r: &impl Read, reads: &[ReadOp]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    rmp::encode::write_array_len(&mut out, reads.len() as u32).expect("writing to a Vec doesn't fail");
    for read in reads {
        let at = out.len();
        answer(r, read, &mut out, &mut Visited::default())?;
        debug_assert!(out.len() > at);
    }
    Ok(out)
}

/// Runs one plan, answering its envelope and what it visited
#[cfg(test)]
pub fn run_counted(r: &impl Read, plan: &Plan) -> Result<(Vec<u8>, Visited)> {
    let mut out = Vec::new();
    let mut visited = Visited::default();
    answer(r, &ReadOp::Query { plan: plan.clone(), visits: false, explain: false }, &mut out, &mut visited)?;
    Ok((out, visited))
}

/// Writes msgpack into a buffer; writing to a `Vec` doesn't fail
struct W<'a>(&'a mut Vec<u8>);

impl W<'_> {
    fn map(&mut self, len: u32) {
        rmp::encode::write_map_len(self.0, len).expect("a Vec");
    }
    fn array(&mut self, len: u32) {
        rmp::encode::write_array_len(self.0, len).expect("a Vec");
    }
    fn str(&mut self, s: &str) {
        rmp::encode::write_str(self.0, s).expect("a Vec");
    }
    fn nil(&mut self) {
        rmp::encode::write_nil(self.0).expect("a Vec");
    }
    fn bool(&mut self, b: bool) {
        rmp::encode::write_bool(self.0, b).expect("a Vec");
    }
    fn uint(&mut self, n: u64) {
        rmp::encode::write_uint(self.0, n).expect("a Vec");
    }
    fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    fn kind(&mut self, kind: &str) {
        self.str("kind");
        self.str(kind);
    }
}

/// Whether any map in these bytes might hold a key `__proto__`, which a decode straight to objects would mistake for a
/// prototype; a false positive only costs the caller the exact decode
fn may_hold_proto(bytes: &[u8]) -> bool {
    bytes.windows(9).any(|w| w == b"__proto__")
}

fn answer(r: &impl Read, read: &ReadOp, out: &mut Vec<u8>, visited: &mut Visited) -> Result<()> {
    match read {
        ReadOp::Query { plan, explain: true, .. } => {
            let stages = Stages::new(plan);
            let mut w = W(out);
            w.map(5);
            w.kind("explained");
            w.str("answer");
            query_with(r, plan, w.0, visited, Some(&stages))?;
            stages.write(plan, visited.entities, &mut W(out));
            Ok(())
        }
        ReadOp::Query { plan, visits: false, .. } => query(r, plan, out, visited),
        ReadOp::Query { plan, visits: true, .. } => {
            let mut w = W(out);
            w.map(3);
            w.kind("visited");
            w.str("answer");
            query(r, plan, w.0, visited)?;
            let mut w = W(out);
            w.str("visited");
            w.uint(visited.entities);
            Ok(())
        }
        ReadOp::Row { id } => {
            let mut w = W(out);
            w.map(3);
            w.kind("row");
            w.str("row");
            let start = w.0.len();
            if entities::record(r, id)?.is_some() {
                write_row(r, id, None, &mut w)?;
            } else {
                w.nil();
            }
            let plain = !may_hold_proto(&w.0[start..]);
            w.str("plainObjects");
            w.bool(plain);
            Ok(())
        }
        ReadOp::Field { id, field: name } => {
            let mut w = W(out);
            w.map(3);
            w.kind("field");
            match get(r, id, name)? {
                Some(bytes) => {
                    w.str("present");
                    w.bool(true);
                    w.str("value");
                    w.raw(&bytes);
                }
                None => {
                    w.str("present");
                    w.bool(false);
                    w.str("value");
                    w.nil();
                }
            }
            Ok(())
        }
        ReadOp::Relation { id } => {
            let mut w = W(out);
            w.map(2);
            w.kind("relation");
            w.str("relation");
            match relations::get(r, id)? {
                Some(stored) => write_relation(id, &stored, &mut w),
                None => w.nil(),
            }
            Ok(())
        }
        ReadOp::FindRelations { kind, source, target } => {
            if let Some(kind) = kind {
                relations::definition(r, kind)?;
            }
            let found = find_relations(r, kind.as_deref(), source.as_deref(), target.as_deref())?;
            let mut w = W(out);
            w.map(2);
            w.kind("relations");
            w.str("relations");
            w.array(found.len() as u32);
            for (id, stored) in &found {
                write_relation(id, stored, &mut w);
            }
            Ok(())
        }
        ReadOp::Suggest { entity_type, index, term, limit } => {
            let found = text::resolve(r.schema(), entity_type, index.as_deref(), "suggest")?;
            let (completions, corrections) = text::search::suggest(r, entity_type, &found, term, *limit as usize)?;
            let mut w = W(out);
            w.map(3);
            w.kind("suggestions");
            for (name, terms) in [("completions", &completions), ("corrections", &corrections)] {
                w.str(name);
                w.array(terms.len() as u32);
                for t in terms {
                    w.str(t);
                }
            }
            Ok(())
        }
        ReadOp::FindLabels { query, fields, limit, scope } => {
            find_labels(r, query, fields, *limit, *scope, &mut W(out))
        }
        ReadOp::PendingEmbeddings { entity_type, index, limit } => {
            let def = super::vector::resolve(r.schema(), entity_type, index.as_deref(), "pendingEmbeddings")?;
            let (chunks, count) = super::vector::pending(r, entity_type, &def.name, *limit as usize)?;
            let mut w = W(out);
            w.map(6);
            w.kind("pending");
            w.str("index");
            w.str(&def.name);
            w.str("model");
            w.str(&def.model);
            w.str("dims");
            w.uint(def.dims as u64);
            w.str("count");
            w.uint(count);
            w.str("chunks");
            w.array(chunks.len() as u32);
            for c in &chunks {
                w.map(4);
                w.str("id");
                w.str(&c.id);
                w.str("chunk");
                w.uint(u64::from(c.chunk));
                w.str("hash");
                w.str(&c.hash);
                w.str("text");
                w.str(&c.text);
            }
            Ok(())
        }
        ReadOp::WouldCreateCycle { source, target, kinds } => {
            for kind in kinds {
                relations::definition(r, kind)?;
            }
            let cycle = relations::would_cycle(r, source, target, kinds)?;
            let mut w = W(out);
            w.map(2);
            w.kind("bool");
            w.str("value");
            w.bool(cycle);
            Ok(())
        }
    }
}

fn write_relation(id: &str, s: &relations::Stored, w: &mut W<'_>) {
    w.map(5);
    w.str("id");
    w.str(id);
    w.str("kind");
    w.str(&s.kind);
    w.str("source");
    w.str(&s.source);
    w.str("target");
    w.str(&s.target);
    w.str("info");
    match &s.info {
        Some(info) => w.raw(&values::encode(info)),
        None => w.nil(),
    }
}

/// A field's msgpack, decompressed where it is stored compressed; `entityType` is read from the id of an entity that exists
fn get(r: &impl Read, id: &str, name: &str) -> Result<Option<Vec<u8>>> {
    if name == field::ENTITY_TYPE {
        return Ok(entities::record(r, id)?.map(|_| values::encode(&ids::type_of(id).into())));
    }
    r.get(&keys::value(id, name))?.map(|bytes| Ok(values::unstored(&bytes)?.into_owned())).transpose()
}

fn get_value(r: &impl Read, id: &str, name: &str) -> Result<Option<Value>> {
    get(r, id, name)?.map(|b| values::decode(&b)).transpose()
}

/// An entity's row, `{ id, entityType, …fields }`, or with `fields`, `{ id, …those it holds }`
fn write_row(r: &impl Read, id: &str, fields: Option<&[String]>, w: &mut W<'_>) -> Result<()> {
    match fields {
        None => {
            let stored = entities::fields(r, id)?;
            w.map(stored.len() as u32 + 2);
            w.str(field::ID);
            w.str(id);
            w.str(field::ENTITY_TYPE);
            w.str(ids::type_of(id));
            for (name, bytes) in &stored {
                w.str(name);
                w.raw(bytes);
            }
        }
        Some(names) => {
            let mut held = Vec::new();
            for name in names.iter().filter(|n| *n != field::ID) {
                if let Some(bytes) = get(r, id, name)? {
                    held.push((name, bytes));
                }
            }
            w.map(held.len() as u32 + 1);
            w.str(field::ID);
            w.str(id);
            for (name, bytes) in &held {
                w.str(name);
                w.raw(bytes);
            }
        }
    }
    Ok(())
}

fn find_relations(
    r: &impl Read,
    kind: Option<&str>,
    source: Option<&str>,
    target: Option<&str>,
) -> Result<Vec<(String, relations::Stored)>> {
    let mut rows = Vec::new();
    let row = |id: String| -> Result<(String, relations::Stored)> {
        let stored = relations::get(r, &id)?
            .ok_or_else(|| Error::new(ErrorCode::Corrupted, format!("Edge of {id} has no row")))?;
        Ok((id, stored))
    };
    match (source, target, kind) {
        (Some(s), Some(t), Some(k)) => {
            for id in relations::between(r, s, k, t)? {
                rows.push(row(id)?);
            }
        }
        (Some(_), _, _) | (_, Some(_), _) => {
            let outward = source.is_some();
            let end = source.or(target).expect("one end is named");
            for (id, far) in relations::edges(r, end, outward, kind)? {
                if outward && target.is_some_and(|t| t != far) {
                    continue;
                }
                rows.push(row(id)?);
            }
        }
        (None, None, _) => {
            for (key, bytes) in r.scan(&keys::all_relations())? {
                let id = match decode_key(&key)?.pop() {
                    Some(Part::String(id)) => id,
                    _ => continue,
                };
                let stored: relations::Stored = rmp_serde::from_slice(&bytes)
                    .map_err(|e| Error::new(ErrorCode::Corrupted, format!("Relation {id}'s row doesn't read: {e}")))?;
                if kind.is_none_or(|k| k == stored.kind) {
                    rows.push((id, stored));
                }
            }
        }
    }
    Ok(rows)
}

/// A query over one read
/// The most steps a plan holds: a long plan's walk would overflow a reading thread's stack, and no real query is near it
pub const MAX_STEPS: usize = 256;

struct Query<'r, R: Read> {
    r: &'r R,
    plan: &'r Plan,
    visited: Cell<u64>,
    /// What each stage did, for a read asked to explain itself
    stages: Option<&'r Stages>,
    /// What the plan's search found in each entity, with the index it searched, for the `hits` terminal
    hits: RefCell<HashMap<String, (Hit, String)>>,
}

type Visit<'a> = dyn FnMut(&str) -> Result<bool> + 'a;

impl<R: Read> Query<'_, R> {
    /// One entity read from the store, by the start or by a step reading an index or walking a type
    fn count_visit(&self) {
        self.visited.set(self.visited.get() + 1);
        if let Some(stages) = self.stages {
            stages.read();
        }
    }

    /// Whether a step may reach the entity: in the store, and out of the trash for a live query
    fn alive(&self, id: &str) -> Result<bool> {
        Ok(match entities::record(self.r, id)? {
            None => false,
            Some(rec) => self.plan.scope != Scope::Live || !rec.trashed,
        })
    }

    /// Whether the start may begin from the entity
    fn admits(&self, id: &str) -> Result<bool> {
        Ok(match entities::record(self.r, id)? {
            None => false,
            Some(rec) => match self.plan.scope {
                Scope::Live => !rec.trashed,
                Scope::WithTrashed => true,
                Scope::Trashed => rec.trashed,
            },
        })
    }

    /// Visits a type's entities in creation order, in the scope's ranges; false when `visit` stopped it. A walk goes
    /// through its range as it visits, so one stopped early reads no further; a walk of both ranges collects the
    /// trash, which is the smaller, and merges it into the live range by place
    fn walk_type(&self, entity_type: &str, visit: &mut Visit<'_>) -> Result<bool> {
        let entry = |key: &[u8], id: &[u8]| -> Result<(u64, String)> {
            let seq = match decode_key(key)?.pop() {
                Some(Part::Number(n)) => n as u64,
                _ => 0,
            };
            Ok((seq, values::decode(id)?.as_str().unwrap_or_default().to_owned()))
        };
        let mut trash: std::collections::VecDeque<(u64, String)> = Default::default();
        if self.plan.scope == Scope::WithTrashed {
            for (key, id) in self.r.scan(&keys::type_range(true, entity_type))? {
                trash.push_back(entry(&key, &id)?);
            }
        }
        let mut go = |id: &str| -> Result<bool> {
            self.count_visit();
            visit(id)
        };
        let mut outcome: Result<bool> = Ok(true);
        let range = keys::type_range(self.plan.scope == Scope::Trashed, entity_type);
        self.r.visit(&range, |key, id| {
            let step = (|| -> Result<bool> {
                let (seq, id) = entry(key, id)?;
                while trash.front().is_some_and(|(t, _)| *t < seq) {
                    let (_, trashed) = trash.pop_front().expect("a front entry");
                    if !go(&trashed)? {
                        return Ok(false);
                    }
                }
                go(&id)
            })();
            match step {
                Ok(true) => true,
                other => {
                    outcome = other;
                    false
                }
            }
        })?;
        if !outcome? {
            return Ok(false);
        }
        while let Some((_, trashed)) = trash.pop_front() {
            if !go(&trashed)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn start_types(&self) -> Option<&[String]> {
        match &self.plan.start {
            Start::Types { types } => Some(types),
            _ => None,
        }
    }

    /// Visits the ids after the first `upto` steps; false when `visit` stopped the walk
    fn pull(&self, upto: usize, visit: &mut Visit<'_>) -> Result<bool> {
        let Some(stages) = self.stages else { return self.stage(upto, visit) };
        // Stage `upto` runs until it hands an id on, and the stage it hands to runs until it returns
        let _running = stages.enter(upto);
        self.stage(upto, &mut |id| {
            stages.kept(upto);
            let _receiving = stages.enter(upto + 1);
            visit(id)
        })
    }

    /// Stage `upto` of the plan: its start, or its `upto`th step over the stages before it
    fn stage(&self, upto: usize, visit: &mut Visit<'_>) -> Result<bool> {
        if upto == 0 {
            return match &self.plan.start {
                Start::Types { types } => {
                    for t in types {
                        if !self.walk_type(t, visit)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                }
                Start::Ids { ids } => {
                    for id in ids {
                        if self.admits(id)? {
                            self.count_visit();
                            if !visit(id)? {
                                return Ok(false);
                            }
                        }
                    }
                    Ok(true)
                }
                Start::All => {
                    let mut all: Vec<(u64, String)> = Vec::new();
                    for (key, bytes) in self.r.scan(&keys::all_entities())? {
                        let id = match decode_key(&key)?.pop() {
                            Some(Part::String(id)) => id,
                            _ => continue,
                        };
                        let seq = values::decode(&bytes)?
                            .as_array()
                            .and_then(|a| a.first())
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        all.push((seq, id));
                    }
                    all.sort_by_key(|(seq, _)| *seq);
                    for (_, id) in all {
                        if self.admits(&id)? {
                            self.count_visit();
                            if !visit(&id)? {
                                return Ok(false);
                            }
                        }
                    }
                    Ok(true)
                }
            };
        }
        let step = &self.plan.steps[upto - 1];
        let before = upto - 1;
        match step {
            Step::OfType { entity_type } => {
                let head = format!("{entity_type}-");
                self.pull(before, &mut |id| if id.starts_with(&head) { visit(id) } else { Ok(true) })
            }
            Step::Within { ids } => {
                let keep: HashSet<&str> = ids.iter().map(String::as_str).collect();
                self.pull(before, &mut |id| if keep.contains(id) { visit(id) } else { Ok(true) })
            }
            Step::Where { field: name, value } => {
                if before == 0
                    && let Some(types) = self.start_types()
                    && let Some(part) = indexable(name, value)
                {
                    return self.holding(types, name, &part, value, visit);
                }
                if self.driver() == Some(before)
                    && let Some(types) = self.start_types()
                    && let Some(part) = indexable(name, value)
                {
                    self.roles_declared(before)?;
                    return self.holding(types, name, &part, value, &mut |id| self.past_filters(before, id, visit));
                }
                self.pull(before, &mut |id| if self.matches(id, name, value)? { visit(id) } else { Ok(true) })
            }
            Step::Has { field: name } => {
                self.pull(before, &mut |id| if get(self.r, id, name)?.is_some() { visit(id) } else { Ok(true) })
            }
            Step::Lacks { field: name } => {
                self.pull(before, &mut |id| if get(self.r, id, name)?.is_none() { visit(id) } else { Ok(true) })
            }
            Step::WithRole { role } => {
                if !self.r.schema().roles.contains_key(role) {
                    return Err(Error::new(
                        ErrorCode::UnknownRole,
                        format!("withRole(\"{role}\"): not a role the schema declares"),
                    ));
                }
                let probe = Value::from(role.as_str());
                if before == 0
                    && let Some(types) = self.start_types()
                {
                    return self.holding(types, field::ROLE, &Part::String(role.clone()), &probe, visit);
                }
                if self.driver() == Some(before)
                    && let Some(types) = self.start_types()
                {
                    self.roles_declared(before)?;
                    let part = Part::String(role.clone());
                    return self
                        .holding(types, field::ROLE, &part, &probe, &mut |id| self.past_filters(before, id, visit));
                }
                self.pull(before, &mut |id| if self.matches(id, field::ROLE, &probe)? { visit(id) } else { Ok(true) })
            }
            Step::LinkedWith { other } => {
                let mut linked = HashSet::new();
                for outward in [true, false] {
                    for (_, far) in relations::edges(self.r, other, outward, None)? {
                        linked.insert(far);
                    }
                }
                self.pull(before, &mut |id| if linked.contains(id) { visit(id) } else { Ok(true) })
            }
            Step::LinkedTo { kind, other } => self.by_link(before, kind, other.as_deref(), true, true, visit),
            Step::LinkedFrom { kind, other } => self.by_link(before, kind, other.as_deref(), false, true, visit),
            Step::UnlinkedTo { kind, other } => self.by_link(before, kind, other.as_deref(), true, false, visit),
            Step::UnlinkedFrom { kind, other } => self.by_link(before, kind, other.as_deref(), false, false, visit),
            Step::Out { kinds, types } => self.hop(before, kinds, types.as_deref(), true, visit),
            Step::In { kinds, types } => self.hop(before, kinds, types.as_deref(), false, visit),
            Step::Descendants { kinds, types } => self.reach(before, kinds, types.as_deref(), true, visit),
            Step::Ancestors { kinds, types } => self.reach(before, kinds, types.as_deref(), false, visit),
            Step::OrderBy { field: name, descending } => {
                let ids = self.collect(before)?;
                let collator = collator(&self.r.schema().collation)?;
                let mut keyed = Vec::with_capacity(ids.len());
                for id in ids {
                    let value = get_value(self.r, &id, name)?;
                    keyed.push((sort_key(value.as_ref()), id));
                }
                keyed.sort_by(|(a, _), (b, _)| {
                    let o = compare(&collator, a, b);
                    if *descending { o.reverse() } else { o }
                });
                for (_, id) in keyed {
                    if !visit(&id)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Step::Reverse => {
                let mut ids = self.collect(before)?;
                ids.reverse();
                each(ids, visit)
            }
            Step::Limit { n } => {
                let take = if n.is_nan() { 0 } else { n.floor() as i64 };
                if take <= 0 {
                    return Ok(true);
                }
                if before > 0
                    && let Step::OrderBy { field: name, descending } = &self.plan.steps[before - 1]
                {
                    let first = self.first_ordered(before - 1, name, *descending, take as usize)?;
                    // The `orderBy` stage, which this read for it, kept what it hands on
                    if let Some(stages) = self.stages {
                        for _ in &first {
                            stages.kept(before);
                        }
                    }
                    return each(first, visit);
                }
                let mut taken = 0i64;
                let mut stopped = false;
                self.pull(before, &mut |id| {
                    if !visit(id)? {
                        stopped = true;
                        return Ok(false);
                    }
                    taken += 1;
                    Ok(taken < take)
                })?;
                Ok(!stopped)
            }
            Step::After { id: cursor } => {
                let mut past = false;
                self.pull(before, &mut |id| {
                    if past {
                        return visit(id);
                    }
                    past = id == cursor;
                    Ok(true)
                })
            }
            Step::Search { query, index, options } => self.search(before, query, index.as_deref(), options, visit),
            Step::Containing { text, index, case_insensitive } => self.containing(
                before,
                &Matcher::substring(text, *case_insensitive),
                index.as_deref(),
                "containing",
                visit,
            ),
            Step::Matching { pattern, index } => {
                self.containing(before, &Matcher::pattern(pattern)?, index.as_deref(), "matching", visit)
            }
            Step::Near { vector, index, k } => {
                let found = self.nearest(before, vector, index.as_deref(), *k as usize, None)?;
                self.record_and_visit(found, visit)
            }
            Step::Like { id, index, k } => {
                let entity_type = ids::type_of(id);
                let def = super::vector::resolve(self.r.schema(), entity_type, index.as_deref(), "like")?;
                let Some(own) = super::vector::own_vector(self.r, entity_type, &def, id)? else { return Ok(true) };
                let found = self.nearest(before, &own, Some(&def.name), *k as usize, Some(id))?;
                self.record_and_visit(found, visit)
            }
            Step::Hybrid { query, vector, text_index, vector_index, k, options } => {
                let found = self.hybrid(
                    before,
                    query,
                    vector,
                    text_index.as_deref(),
                    vector_index.as_deref(),
                    *k as usize,
                    options,
                )?;
                self.record_and_visit(found, visit)
            }
            Step::Distinct { field: name } => {
                let ids = self.collect(before)?;
                let mut kept = Vec::new();
                match name {
                    None => {
                        let mut seen = HashSet::new();
                        for id in ids {
                            if seen.insert(id.clone()) {
                                kept.push(id);
                            }
                        }
                    }
                    Some(name) => {
                        let mut seen: Vec<SameValue> = Vec::new();
                        for id in ids {
                            let key = SameValue::of(get_value(self.r, &id, name)?.as_ref());
                            if key.is_unique() || !seen.contains(&key) {
                                seen.push(key);
                                kept.push(id);
                            }
                        }
                    }
                }
                each(kept, visit)
            }
        }
    }

    /// The entities a search matches, most relevant first, ties in the order they came: a type start's whole types,
    /// or the set before the step, of whose types one with no text index holds no match
    fn search(
        &self,
        before: usize,
        query: &str,
        index: Option<&str>,
        options: &SearchOptions,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        let raw = text::search::parse(query);
        let now = options.recency.as_ref().and_then(|r| r.now).unwrap_or_else(super::now_ms);
        let among = match self.start_types() {
            Some(_) if before == 0 => None,
            _ => Some(self.collect(before)?),
        };
        let types: Vec<String> = match &among {
            None => self.start_types().unwrap_or_default().to_vec(),
            Some(ids) => {
                let mut types: Vec<String> = Vec::new();
                for id in ids {
                    let t = ids::type_of(id);
                    if !types.iter().any(|known| known == t) {
                        types.push(t.to_owned());
                    }
                }
                types
            }
        };
        let order: Option<HashMap<&str, usize>> =
            among.as_ref().map(|ids| ids.iter().enumerate().rev().map(|(at, id)| (id.as_str(), at)).collect());
        let mut found: Vec<((usize, u64), Hit, String)> = Vec::new();
        for (at, entity_type) in types.iter().enumerate() {
            let text_index = match text::resolve(self.r.schema(), entity_type, index, "search") {
                Ok(found) => found,
                // Among a set, a type with no text index holds no match; one with several, none named, is refused
                Err(_)
                    if among.is_some()
                        && index.is_none()
                        && text::text_indexes(self.r.schema(), entity_type).is_empty() =>
                {
                    continue;
                }
                Err(e) => return Err(e),
            };
            let within = match &among {
                None => None,
                Some(ids) => {
                    let of_type: Vec<String> =
                        ids.iter().filter(|id| ids::type_of(id) == entity_type).cloned().collect();
                    Some(text::search::places(self.r, &of_type)?.into_keys().collect::<roaring::RoaringTreemap>())
                }
            };
            for hit in text::search::search(self.r, entity_type, &text_index, &raw, options, within.as_ref(), now)? {
                self.count_visit();
                let place = match &order {
                    Some(order) => (order.get(hit.id.as_str()).copied().unwrap_or(usize::MAX), 0),
                    None if self.admits(&hit.id)? => (at, hit.seq),
                    None => continue,
                };
                found.push((place, hit, text_index.name.clone()));
            }
        }
        found.sort_by(|a, b| b.1.score.partial_cmp(&a.1.score).unwrap_or(Ordering::Equal).then_with(|| a.0.cmp(&b.0)));
        let ids: Vec<String> = found.iter().map(|(_, hit, _)| hit.id.clone()).collect();
        {
            let mut hits = self.hits.borrow_mut();
            for (_, hit, name) in found {
                hits.insert(hit.id.clone(), (hit, name));
            }
        }
        each(ids, visit)
    }

    /// The entities whose text in a trigram index matches, in the order they came. A type start's whole types are
    /// read from the index's candidates, each checked, in place order as a walk of the type visits them; an index not
    /// yet built narrows by nothing, so the type is walked. The set before the step is checked entity by entity, and
    /// one of a type with no trigram index matches nothing, unless the step names an index, which every type must have
    fn containing(
        &self,
        before: usize,
        matcher: &Matcher,
        index: Option<&str>,
        via: &str,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        if before == 0
            && let Some(types) = self.start_types()
        {
            let need = matcher.need();
            for entity_type in types {
                let found = trigram::resolve(self.r.schema(), entity_type, index, via)?;
                let narrowed = match super::derive::is_built(self.r, entity_type, &found.name)? {
                    true => trigram::candidates(self.r, entity_type, &found.name, &need)?,
                    false => None,
                };
                let mut keep = |id: &str| -> Result<bool> {
                    if matcher.matches(self.r, &found, id)? { visit(id) } else { Ok(true) }
                };
                let Some(places) = narrowed else {
                    if !self.walk_type(entity_type, &mut keep)? {
                        return Ok(false);
                    }
                    continue;
                };
                let ranges: &[bool] = match self.plan.scope {
                    Scope::Live => &[false],
                    Scope::Trashed => &[true],
                    Scope::WithTrashed => &[false, true],
                };
                for seq in places {
                    for &trashed in ranges {
                        let Some(held) = self.r.get(&keys::of_type(trashed, entity_type, seq))? else { continue };
                        self.count_visit();
                        if !keep(values::decode(&held)?.as_str().unwrap_or_default())? {
                            return Ok(false);
                        }
                    }
                }
            }
            return Ok(true);
        }
        self.pull(before, &mut |id| {
            let found = match trigram::resolve(self.r.schema(), ids::type_of(id), index, via) {
                Ok(found) => found,
                // A type with no trigram index holds no match; one with several, none named, is refused
                Err(_) if index.is_none() && trigram::trigram_indexes(self.r.schema(), ids::type_of(id)).is_empty() => {
                    return Ok(true);
                }
                Err(e) => return Err(e),
            };
            if matcher.matches(self.r, &found, id)? { visit(id) } else { Ok(true) }
        })
    }

    /// The types a step searches, and the entities it searches among: a type start's whole types, or the set before
    fn searched(&self, before: usize) -> Result<(Vec<String>, Option<Vec<String>>)> {
        let among = match self.start_types() {
            Some(_) if before == 0 => None,
            _ => Some(self.collect(before)?),
        };
        let types = match &among {
            None => self.start_types().unwrap_or_default().to_vec(),
            Some(ids) => {
                let mut types: Vec<String> = Vec::new();
                for id in ids {
                    let t = ids::type_of(id);
                    if !types.iter().any(|known| known == t) {
                        types.push(t.to_owned());
                    }
                }
                types
            }
        };
        Ok((types, among))
    }

    /// The `k` entities nearest `vector` among the searched types' vector indexes, nearest first, as hits scored by
    /// nearness (`1 / (1 + distance)`), `except` left out
    fn nearest(
        &self,
        before: usize,
        vector: &[f32],
        index: Option<&str>,
        k: usize,
        except: Option<&str>,
    ) -> Result<Vec<(Hit, String)>> {
        let (types, among) = self.searched(before)?;
        let mut found: Vec<(f32, Hit, String)> = Vec::new();
        for entity_type in &types {
            let def = match super::vector::resolve(self.r.schema(), entity_type, index, "near") {
                Ok(def) => def,
                Err(_)
                    if among.is_some()
                        && index.is_none()
                        && super::vector::vector_indexes(self.r.schema(), entity_type).is_empty() =>
                {
                    continue;
                }
                Err(e) => return Err(e),
            };
            let within = match &among {
                None => None,
                Some(ids) => {
                    let of_type: Vec<String> =
                        ids.iter().filter(|id| ids::type_of(id) == entity_type).cloned().collect();
                    Some(super::vector::items_of(self.r, entity_type, &def.name, &of_type)?)
                }
            };
            // The trash, the scope and `except` are left out after the graph answers, so it is asked for more than `k`,
            // and asked again for more while what it gave was not enough and it held more
            let mut ask = k + 8;
            loop {
                let (nearest, exhausted) =
                    super::vector::nearest(self.r, entity_type, &def, vector, ask, within.as_ref())?;
                let mut admitted = Vec::new();
                for (id, distance) in nearest {
                    self.count_visit();
                    if except == Some(id.as_str()) || (among.is_none() && !self.admits(&id)?) {
                        continue;
                    }
                    let score = 1.0 / (1.0 + f64::from(distance));
                    admitted.push((distance, Hit { id, seq: 0, score, terms: Vec::new(), fields: Vec::new() }));
                }
                if admitted.len() >= k || exhausted {
                    found.extend(admitted.into_iter().map(|(distance, hit)| (distance, hit, def.name.clone())));
                    break;
                }
                ask = ask.saturating_mul(4);
            }
        }
        found.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        found.truncate(k);
        Ok(found.into_iter().map(|(_, hit, name)| (hit, name)).collect())
    }

    /// A text search and a vector search fused by reciprocal rank (each list's rank `r` adds `1 / (60 + r)`), the
    /// `k` best; a hit keeps the text search's matched terms, and so its highlights
    #[allow(clippy::too_many_arguments)]
    fn hybrid(
        &self,
        before: usize,
        query: &str,
        vector: &[f32],
        text_index: Option<&str>,
        vector_index: Option<&str>,
        k: usize,
        options: &SearchOptions,
    ) -> Result<Vec<(Hit, String)>> {
        const FUSION: f64 = 60.0;
        let wide = (k * 4).max(40);
        let near = self.nearest(before, vector, vector_index, wide, None)?;
        let (types, among) = self.searched(before)?;
        let raw = text::search::parse(query);
        let now = options.recency.as_ref().and_then(|r| r.now).unwrap_or_else(super::now_ms);
        let mut texts: Vec<(Hit, String)> = Vec::new();
        for entity_type in &types {
            let def = match text::resolve(self.r.schema(), entity_type, text_index, "hybrid") {
                Ok(def) => def,
                Err(_)
                    if among.is_some()
                        && text_index.is_none()
                        && text::text_indexes(self.r.schema(), entity_type).is_empty() =>
                {
                    continue;
                }
                Err(e) => return Err(e),
            };
            let within = match &among {
                None => None,
                Some(ids) => {
                    let of_type: Vec<String> =
                        ids.iter().filter(|id| ids::type_of(id) == entity_type).cloned().collect();
                    Some(text::search::places(self.r, &of_type)?.into_keys().collect::<roaring::RoaringTreemap>())
                }
            };
            for hit in text::search::search(self.r, entity_type, &def, &raw, options, within.as_ref(), now)? {
                if among.is_none() && !self.admits(&hit.id)? {
                    continue;
                }
                texts.push((hit, def.name.clone()));
            }
        }
        texts.sort_by(|a, b| b.0.score.partial_cmp(&a.0.score).unwrap_or(Ordering::Equal));
        let mut fused: Vec<(f64, Hit, String)> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for list in [texts, near] {
            for (rank, (hit, name)) in list.into_iter().enumerate() {
                let add = 1.0 / (FUSION + rank as f64 + 1.0);
                match at.get(&hit.id) {
                    Some(&i) => {
                        fused[i].0 += add;
                        if fused[i].1.terms.is_empty() {
                            fused[i].1.terms = hit.terms;
                            fused[i].1.fields = hit.fields;
                            fused[i].2 = name;
                        }
                    }
                    None => {
                        at.insert(hit.id.clone(), fused.len());
                        fused.push((add, hit, name));
                    }
                }
            }
        }
        fused.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
        fused.truncate(k);
        Ok(fused
            .into_iter()
            .map(|(score, mut hit, name)| {
                hit.score = score;
                (hit, name)
            })
            .collect())
    }

    /// Records each hit for the `hits` terminal and visits its entity, in the order given
    fn record_and_visit(&self, found: Vec<(Hit, String)>, visit: &mut Visit<'_>) -> Result<bool> {
        let ids: Vec<String> = found.iter().map(|(hit, _)| hit.id.clone()).collect();
        {
            let mut hits = self.hits.borrow_mut();
            for (hit, name) in found {
                hits.insert(hit.id.clone(), (hit, name));
            }
        }
        each(ids, visit)
    }

    /// The step a type start's leading filters are read from: the first `where` or `withRole` among the filters the
    /// plan starts with that every start type has a built index for, when no step before it is one. The filters
    /// before it keep the same set in any order, so the index's matches are read and the earlier filters checked
    /// against each, in the creation order a walk of the type would have visited them
    fn driver(&self) -> Option<usize> {
        let types = self.start_types()?;
        for (at, step) in self.plan.steps.iter().enumerate() {
            let field = match step {
                Step::Where { field: name, value } => indexable(name, value).map(|_| name.as_str()),
                Step::WithRole { .. } => Some(field::ROLE),
                Step::OfType { .. } | Step::Within { .. } | Step::Has { .. } | Step::Lacks { .. } => None,
                _ => return None,
            };
            if let Some(name) = field
                && types.iter().all(|t| index_on(self.r, t, name).is_some())
            {
                return Some(at);
            }
        }
        None
    }

    /// Refuses a `withRole` among the filters before a driving index that names no declared role, as the step itself
    /// does when it runs
    fn roles_declared(&self, upto: usize) -> Result<()> {
        for step in &self.plan.steps[..upto] {
            if let Step::WithRole { role } = step
                && !self.r.schema().roles.contains_key(role)
            {
                return Err(Error::new(
                    ErrorCode::UnknownRole,
                    format!("withRole(\"{role}\"): not a role the schema declares"),
                ));
            }
        }
        Ok(())
    }

    /// Visits `id` if every one of the first `upto` steps, the filters before a driving index, keeps it
    fn past_filters(&self, upto: usize, id: &str, visit: &mut Visit<'_>) -> Result<bool> {
        for step in &self.plan.steps[..upto] {
            let kept = match step {
                Step::OfType { entity_type } => {
                    id.strip_prefix(entity_type.as_str()).is_some_and(|rest| rest.starts_with('-'))
                }
                Step::Within { ids } => ids.iter().any(|kept| kept == id),
                Step::Where { field: name, value } => self.matches(id, name, value)?,
                Step::Has { field: name } => get(self.r, id, name)?.is_some(),
                Step::Lacks { field: name } => get(self.r, id, name)?.is_none(),
                Step::WithRole { role } => self.matches(id, field::ROLE, &Value::from(role.as_str()))?,
                _ => unreachable!("a driving index is preceded only by filters"),
            };
            if !kept {
                return Ok(true);
            }
        }
        visit(id)
    }

    /// The first `take` ids after the first `upto` steps in `name`'s order, as sorting them all and taking the first
    /// would answer, equal values in the order they came: only `take` are held at once
    fn first_ordered(&self, upto: usize, name: &str, descending: bool, take: usize) -> Result<Vec<String>> {
        let collator = collator(&self.r.schema().collation)?;
        let order = |a: &SortKey, b: &SortKey| {
            let o = compare(&collator, a, b);
            if descending { o.reverse() } else { o }
        };
        // Ids are held as they come, up to twice `take`, then sorted (stably, so the earlier of two equals stays first)
        // and cut to `take`: each id costs a log of `take`, whatever order the ids come in
        let mut kept: Vec<(SortKey, String)> = Vec::with_capacity(take.min(1024) * 2 + 1);
        // The last kept at the last cut: one ordered at or after it can't be among the first, an equal one having
        // come later
        let mut bound: Option<SortKey> = None;
        let cut = |kept: &mut Vec<(SortKey, String)>| {
            kept.sort_by(|(a, _), (b, _)| order(a, b));
            kept.truncate(take);
        };
        self.pull(upto, &mut |id| {
            let key = sort_key(get_value(self.r, id, name)?.as_ref());
            if bound.as_ref().is_some_and(|last| order(last, &key) != Ordering::Greater) {
                return Ok(true);
            }
            kept.push((key, id.to_owned()));
            if kept.len() >= take.saturating_mul(2).max(1) {
                cut(&mut kept);
                bound = (kept.len() == take).then(|| kept[take - 1].0.clone());
            }
            Ok(true)
        })?;
        cut(&mut kept);
        Ok(kept.into_iter().map(|(_, id)| id).collect())
    }

    /// Every id after the first `upto` steps
    fn collect(&self, upto: usize) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        self.pull(upto, &mut |id| {
            ids.push(id.to_owned());
            Ok(true)
        })?;
        Ok(ids)
    }

    /// Whether the entity's field is `probe`, or a list holding it; a null probe matches a missing field too
    fn matches(&self, id: &str, name: &str, probe: &Value) -> Result<bool> {
        let stored = get_value(self.r, id, name)?;
        Ok(match stored {
            None => probe.is_nil(),
            Some(v) => values::holds(&v, probe),
        })
    }

    /// The start's entities holding `probe` in `name`: from an index of the type covering the field, in creation order,
    /// or by walking the type's rows
    fn holding(&self, types: &[String], name: &str, part: &Part, probe: &Value, visit: &mut Visit<'_>) -> Result<bool> {
        for t in types {
            let index = index_on(self.r, t, name);
            if let Some(stages) = self.stages {
                stages.reads_index(index.as_deref().unwrap_or("none: walks the type"));
            }
            match index {
                Some(index) => {
                    // One value's entries are keyed by place, so the range is already in creation order, an entity once:
                    // it is visited as it is read, and a step stopped early reads no further
                    let mut outcome: Result<bool> = Ok(true);
                    self.r.visit(&keys::index_range(t, &index, std::slice::from_ref(part)), |_, id| {
                        let step = (|| -> Result<bool> {
                            let id = values::decode(id)?.as_str().unwrap_or_default().to_owned();
                            self.count_visit();
                            if self.admits(&id)? { visit(&id) } else { Ok(true) }
                        })();
                        match step {
                            Ok(true) => true,
                            other => {
                                outcome = other;
                                false
                            }
                        }
                    })?;
                    if !outcome? {
                        return Ok(false);
                    }
                }
                None => {
                    let ended =
                        self.walk_type(t, &mut |id| if self.matches(id, name, probe)? { visit(id) } else { Ok(true) })?;
                    if !ended {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    fn by_link(
        &self,
        before: usize,
        kind: &str,
        other: Option<&str>,
        outward: bool,
        keep: bool,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        relations::definition(self.r, kind)?;
        self.pull(before, &mut |id| {
            let mut linked = false;
            for (_, end) in relations::edges(self.r, id, outward, Some(kind))? {
                let hit = match other {
                    None => end != id && self.alive(&end)?,
                    Some(o) => end == o,
                };
                if hit {
                    linked = true;
                    break;
                }
            }
            if linked == keep { visit(id) } else { Ok(true) }
        })
    }

    fn hop(
        &self,
        before: usize,
        kinds: &[String],
        types: Option<&[String]>,
        outward: bool,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        for kind in kinds {
            relations::definition(self.r, kind)?;
        }
        let mut reached = HashSet::new();
        self.pull(before, &mut |from| {
            for kind in kinds {
                for (_, far) in relations::edges(self.r, from, outward, Some(kind))? {
                    if far == from || !of_types(&far, types) || reached.contains(&far) {
                        continue;
                    }
                    reached.insert(far.clone());
                    if self.alive(&far)? && !visit(&far)? {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        })
    }

    fn reach(
        &self,
        before: usize,
        kinds: &[String],
        types: Option<&[String]>,
        outward: bool,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        for kind in kinds {
            relations::definition(self.r, kind)?;
        }
        let mut frontier = self.collect(before)?;
        let mut seen: HashSet<String> = frontier.iter().cloned().collect();
        let mut reached = Vec::new();
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for from in &frontier {
                for kind in kinds {
                    for (_, far) in relations::edges(self.r, from, outward, Some(kind))? {
                        if seen.contains(&far) || !self.alive(&far)? {
                            continue;
                        }
                        seen.insert(far.clone());
                        next.push(far);
                    }
                }
            }
            reached.extend(next.iter().cloned());
            frontier = next;
        }
        each(reached.into_iter().filter(|id| of_types(id, types)).collect(), visit)
    }
}

/// What each stage of one plan did, while a read asked to explain it runs: stage 0 is the start, stage `n` the plan's
/// `n`th step, and the last the terminal. The stage running is the one whose code holds the thread: a stage runs until
/// it hands an id to the next, which runs until it returns, so each stage's time is its own and not its callers'
struct Stages {
    /// Per stage: entities read from the store, ids handed on, the index it read, and nanoseconds spent in it
    read: Vec<Cell<u64>>,
    kept: Vec<Cell<u64>>,
    index: RefCell<Vec<Option<String>>>,
    nanos: Vec<Cell<u64>>,
    running: Cell<usize>,
    since: Cell<std::time::Instant>,
    started: std::time::Instant,
}

/// Puts back the stage that ran before, charging the time since to the one ending
struct Entered<'s> {
    stages: &'s Stages,
    previous: usize,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.stages.switch(self.previous);
    }
}

impl Stages {
    fn new(plan: &Plan) -> Self {
        let n = plan.steps.len() + 2;
        let now = std::time::Instant::now();
        Stages {
            read: (0..n).map(|_| Cell::new(0)).collect(),
            kept: (0..n).map(|_| Cell::new(0)).collect(),
            index: RefCell::new(vec![None; n]),
            nanos: (0..n).map(|_| Cell::new(0)).collect(),
            // Before any step pulls, the terminal is what runs
            running: Cell::new(n - 1),
            since: Cell::new(now),
            started: now,
        }
    }

    fn switch(&self, to: usize) {
        let now = std::time::Instant::now();
        let at = self.running.get();
        self.nanos[at].set(self.nanos[at].get() + (now - self.since.get()).as_nanos() as u64);
        self.since.set(now);
        self.running.set(to);
    }

    fn enter(&self, stage: usize) -> Entered<'_> {
        let previous = self.running.get();
        self.switch(stage);
        Entered { stages: self, previous }
    }

    fn read(&self) {
        let at = self.running.get();
        self.read[at].set(self.read[at].get() + 1);
    }

    fn kept(&self, stage: usize) {
        self.kept[stage].set(self.kept[stage].get() + 1);
    }

    fn reads_index(&self, name: &str) {
        let at = self.running.get();
        let mut index = self.index.borrow_mut();
        index[at] = Some(match index[at].take() {
            Some(held) if held != name => format!("{held}, {name}"),
            _ => name.to_owned(),
        });
    }

    /// The rest of the `explained` answer after its `answer`: `visited`, `ms` and `stages`
    fn write(&self, plan: &Plan, visited: u64, w: &mut W<'_>) {
        self.switch(self.running.get());
        let ms = |nanos: u64| nanos as f64 / 1e6;
        w.str("visited");
        w.uint(visited);
        w.str("ms");
        rmp::encode::write_f64(w.0, ms(self.started.elapsed().as_nanos() as u64)).expect("a Vec");
        w.str("stages");
        let n = self.read.len();
        w.array(n as u32);
        let index = self.index.borrow();
        for at in 0..n {
            let name = match at {
                0 => start_name(&plan.start),
                _ if at == n - 1 => terminal_name(&plan.terminal),
                _ => step_name(&plan.steps[at - 1]),
            };
            // A stage visits what the stage before handed it, and what it read from the store itself
            let received = if at == 0 { 0 } else { self.kept[at - 1].get() };
            w.map(if index[at].is_some() { 5 } else { 4 });
            w.str("stage");
            w.str(name);
            if let Some(index) = &index[at] {
                w.str("index");
                w.str(index);
            }
            w.str("visited");
            w.uint(received + self.read[at].get());
            w.str("kept");
            // The terminal keeps what it answers: what the last step handed it
            w.uint(if at == n - 1 { received } else { self.kept[at].get() });
            w.str("ms");
            rmp::encode::write_f64(w.0, ms(self.nanos[at].get())).expect("a Vec");
        }
    }
}

fn start_name(start: &Start) -> &'static str {
    match start {
        Start::Types { .. } => "types",
        Start::Ids { .. } => "ids",
        Start::All => "all",
    }
}

fn step_name(step: &Step) -> &'static str {
    match step {
        Step::OfType { .. } => "ofType",
        Step::Within { .. } => "within",
        Step::Where { .. } => "where",
        Step::Has { .. } => "has",
        Step::Lacks { .. } => "lacks",
        Step::WithRole { .. } => "withRole",
        Step::LinkedWith { .. } => "linkedWith",
        Step::LinkedTo { .. } => "linkedTo",
        Step::LinkedFrom { .. } => "linkedFrom",
        Step::UnlinkedTo { .. } => "unlinkedTo",
        Step::UnlinkedFrom { .. } => "unlinkedFrom",
        Step::Out { .. } => "out",
        Step::In { .. } => "in",
        Step::Descendants { .. } => "descendants",
        Step::Ancestors { .. } => "ancestors",
        Step::OrderBy { .. } => "orderBy",
        Step::Reverse => "reverse",
        Step::Limit { .. } => "limit",
        Step::Distinct { .. } => "distinct",
        Step::After { .. } => "after",
        Step::Near { .. } => "near",
        Step::Like { .. } => "like",
        Step::Hybrid { .. } => "hybrid",
        Step::Search { .. } => "search",
        Step::Containing { .. } => "containing",
        Step::Matching { .. } => "matching",
    }
}

fn terminal_name(terminal: &Terminal) -> &'static str {
    match terminal {
        Terminal::Ids => "ids",
        Terminal::Count => "count",
        Terminal::First => "first",
        Terminal::Last => "last",
        Terminal::Exists => "exists",
        Terminal::Rows { .. } => "rows",
        Terminal::FirstRow { .. } => "firstRow",
        Terminal::Edges { .. } => "edges",
        Terminal::GroupBy { .. } => "groupBy",
        Terminal::Hits { .. } => "hits",
    }
}

/// A `where` a value index can answer: a value an index holds, on a field that isn't the entity's type
fn indexable(name: &str, value: &Value) -> Option<Part> {
    if name == field::ENTITY_TYPE || value.is_nil() {
        return None;
    }
    values::index_part(value)
}

fn each(ids: Vec<String>, visit: &mut Visit<'_>) -> Result<bool> {
    for id in ids {
        if !visit(&id)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn of_types(id: &str, types: Option<&[String]>) -> bool {
    types.is_none_or(|ts| {
        ts.is_empty() || ts.iter().any(|t| id.strip_prefix(t.as_str()).is_some_and(|rest| rest.starts_with('-')))
    })
}

/// The built value index of the type a lookup of `name` reads: the built-in one for roles and short codes, or a
/// declared one whose first field it is, a compound one's included, since its leading part is a prefix of its entries.
/// An index not yet built isn't read: the lookup walks the type's rows
fn index_on(r: &impl Read, entity_type: &str, name: &str) -> Option<String> {
    let found = match name {
        field::ROLE => Some(ROLE_INDEX.to_owned()),
        field::SHORT_CODE => Some(SHORT_CODE_INDEX.to_owned()),
        _ => r.schema().types.get(entity_type)?.indexes.iter().find_map(|(index_name, index)| match index {
            // A single-field index only: a compound one holds no entry for an entity missing another of its fields
            crate::schema::IndexDef::Value { fields, .. } if fields.len() == 1 && fields[0] == name => {
                Some(index_name.clone())
            }
            _ => None,
        }),
    }?;
    super::derive::is_built(r, entity_type, &found).ok()?.then_some(found)
}

/// A value's place in an order: its rank, then within it a number, a time, a string, or JSON text
#[derive(Debug, Clone)]
enum SortKey {
    Bool(bool),
    Number(f64),
    Time(f64),
    Text(String),
    Json(String),
    /// Missing, or null
    Last,
}

impl SortKey {
    fn rank(&self) -> u8 {
        match self {
            SortKey::Bool(_) => 0,
            SortKey::Number(_) => 1,
            SortKey::Time(_) => 2,
            SortKey::Text(_) => 3,
            SortKey::Json(_) => 4,
            SortKey::Last => 5,
        }
    }
}

fn sort_key(value: Option<&Value>) -> SortKey {
    match value {
        None | Some(Value::Nil) => SortKey::Last,
        Some(Value::Boolean(b)) => SortKey::Bool(*b),
        Some(Value::String(s)) => SortKey::Text(s.as_str().unwrap_or_default().to_owned()),
        Some(Value::Ext(t, data)) if *t == TIMESTAMP_EXTENSION => SortKey::Time(values::timestamp_ms(data)),
        Some(v) => match values::as_number(v) {
            Some(n) => SortKey::Number(n),
            None => SortKey::Json(json_text(v)),
        },
    }
}

fn compare(collator: &icu_collator::CollatorBorrowed<'static>, a: &SortKey, b: &SortKey) -> Ordering {
    a.rank().cmp(&b.rank()).then_with(|| match (a, b) {
        (SortKey::Bool(x), SortKey::Bool(y)) => x.cmp(y),
        (SortKey::Number(x), SortKey::Number(y)) | (SortKey::Time(x), SortKey::Time(y)) => {
            x.partial_cmp(y).unwrap_or(Ordering::Equal)
        }
        (SortKey::Text(x), SortKey::Text(y)) | (SortKey::Json(x), SortKey::Json(y)) => collator.compare(x, y),
        _ => Ordering::Equal,
    })
}

/// The collator a schema's `collation` names: ICU's root, or a locale's
pub fn collator(collation: &str) -> Result<icu_collator::CollatorBorrowed<'static>> {
    let prefs = if collation == "root" {
        icu_collator::CollatorPreferences::default()
    } else {
        let locale: icu_locale_core::Locale = collation
            .parse()
            .map_err(|_| Error::new(ErrorCode::SchemaInvalid, format!("\"{collation}\" isn't a locale")))?;
        icu_collator::CollatorPreferences::from(&locale)
    };
    icu_collator::CollatorBorrowed::try_new(prefs, icu_collator::options::CollatorOptions::default())
        .map_err(|e| Error::new(ErrorCode::SchemaInvalid, format!("No collation for \"{collation}\": {e}")))
}

/// A value's JSON text as JavaScript's `JSON.stringify` writes it, keys in their stored order
fn json_text(v: &Value) -> String {
    match v {
        Value::Nil => "null".to_owned(),
        Value::Boolean(b) => b.to_string(),
        Value::String(s) => serde_json::to_string(s.as_str().unwrap_or_default()).expect("a string is JSON"),
        Value::Array(items) => format!("[{}]", items.iter().map(json_text).collect::<Vec<_>>().join(",")),
        Value::Map(entries) => format!(
            "{{{}}}",
            entries.iter().map(|(k, v)| format!("{}:{}", json_text(k), json_text(v))).collect::<Vec<_>>().join(",")
        ),
        Value::Ext(..) | Value::Binary(_) => "{}".to_owned(),
        // JSON has no infinity or NaN, which `JSON.stringify` writes as null
        other => match values::as_number(other) {
            Some(n) if n.is_finite() => ryu_js::Buffer::new().format_finite(n).to_owned(),
            _ => "null".to_owned(),
        },
    }
}

/// A key as JavaScript's `Set` and `Map` compare them: scalars by value, and no list, map or timestamp equal to any
/// other, since each read decodes a new object
#[derive(Debug, Clone, PartialEq)]
enum SameValue {
    Scalar(Value),
    Unique,
}

impl SameValue {
    fn of(value: Option<&Value>) -> Self {
        match value {
            None | Some(Value::Nil) => SameValue::Scalar(Value::Nil),
            Some(Value::Array(_) | Value::Map(_) | Value::Ext(..) | Value::Binary(_)) => SameValue::Unique,
            Some(v) => match values::as_number(v) {
                Some(n) => SameValue::Scalar(Value::from(n)),
                None => SameValue::Scalar(v.clone()),
            },
        }
    }

    fn is_unique(&self) -> bool {
        matches!(self, SameValue::Unique)
    }
}

fn query(r: &impl Read, plan: &Plan, out: &mut Vec<u8>, visited: &mut Visited) -> Result<()> {
    query_with(r, plan, out, visited, None)
}

fn query_with(
    r: &impl Read,
    plan: &Plan,
    out: &mut Vec<u8>,
    visited: &mut Visited,
    stages: Option<&Stages>,
) -> Result<()> {
    if let Start::Types { types } = &plan.start {
        let unknown: Vec<&String> = types.iter().filter(|t| !r.schema().types.contains_key(*t)).collect();
        if !unknown.is_empty() {
            let names: Vec<String> = unknown.iter().map(|t| format!("\"{t}\"")).collect();
            return Err(Error::new(
                ErrorCode::UnknownType,
                format!(
                    "query({}): not a type the schema declares. query.ids(...) starts from ids, and query.all() from every entity.",
                    names.join(", ")
                ),
            ));
        }
    }
    // A step is a frame of the walk that pulls ids through the steps before it, so a plan is held to a depth the
    // stack of a reading thread carries
    if plan.steps.len() > MAX_STEPS {
        return Err(Error::new(
            ErrorCode::BatchInvalid,
            format!("A plan holds at most {MAX_STEPS} steps, and this one {}", plan.steps.len()),
        ));
    }
    let q = Query { r, plan, visited: Cell::new(0), stages, hits: RefCell::new(HashMap::new()) };
    let last = plan.steps.len();
    let mut w = W(out);
    match &plan.terminal {
        Terminal::Ids => {
            let ids = q.collect(last)?;
            w.map(2);
            w.kind("ids");
            w.str("ids");
            w.array(ids.len() as u32);
            for id in &ids {
                w.str(id);
            }
        }
        Terminal::Count => {
            // A type's live entities are counted as they enter and leave: one read where any step would need the walk
            let n = match (&plan.start, plan.scope, plan.steps.is_empty()) {
                (Start::Types { types }, Scope::Live, true) => {
                    let mut n = 0;
                    for t in types {
                        n += entities::count(r, t)?;
                    }
                    n
                }
                _ => {
                    let mut n = 0u64;
                    q.pull(last, &mut |_| {
                        n += 1;
                        Ok(true)
                    })?;
                    n
                }
            };
            w.map(2);
            w.kind("count");
            w.str("n");
            w.uint(n);
        }
        Terminal::First | Terminal::Exists => {
            let mut first = None;
            q.pull(last, &mut |id| {
                first = Some(id.to_owned());
                Ok(false)
            })?;
            w.map(2);
            if plan.terminal == Terminal::Exists {
                w.kind("bool");
                w.str("value");
                w.bool(first.is_some());
            } else {
                w.kind("id");
                w.str("id");
                match &first {
                    Some(id) => w.str(id),
                    None => w.nil(),
                }
            }
        }
        Terminal::Last => {
            let ids = q.collect(last)?;
            w.map(2);
            w.kind("id");
            w.str("id");
            match ids.last() {
                Some(id) => w.str(id),
                None => w.nil(),
            }
        }
        Terminal::Rows { fields } => {
            let ids = q.collect(last)?;
            w.map(3);
            w.kind("rows");
            w.str("rows");
            let start = w.0.len();
            w.array(ids.len() as u32);
            for id in &ids {
                write_row(r, id, fields.as_deref(), &mut w)?;
            }
            let plain = !may_hold_proto(&w.0[start..]);
            w.str("plainObjects");
            w.bool(plain);
        }
        Terminal::FirstRow { fields } => {
            let mut first = None;
            q.pull(last, &mut |id| {
                first = Some(id.to_owned());
                Ok(false)
            })?;
            w.map(3);
            w.kind("row");
            w.str("row");
            let start = w.0.len();
            match &first {
                Some(id) => write_row(r, id, fields.as_deref(), &mut w)?,
                None => w.nil(),
            }
            let plain = !may_hold_proto(&w.0[start..]);
            w.str("plainObjects");
            w.bool(plain);
        }
        Terminal::Edges { kinds, outward, types } => {
            for kind in kinds {
                relations::definition(r, kind)?;
            }
            let ids = q.collect(last)?;
            let mut rows = Vec::new();
            for from in &ids {
                for kind in kinds {
                    for (relation, far) in relations::edges(r, from, *outward, Some(kind))? {
                        if of_types(&far, types.as_deref()) && q.alive(&far)? {
                            let stored = relations::get(r, &relation)?.expect("an edge's relation exists");
                            rows.push((relation, stored));
                        }
                    }
                }
            }
            w.map(2);
            w.kind("relations");
            w.str("relations");
            w.array(rows.len() as u32);
            for (id, stored) in &rows {
                write_relation(id, stored, &mut w);
            }
        }
        Terminal::GroupBy { field: name, counts } => {
            let ids = q.collect(last)?;
            let mut groups: Vec<(SameValue, Vec<u8>, Vec<String>)> = Vec::new();
            for id in ids {
                let bytes = get(r, &id, name)?;
                let value = bytes.as_ref().map(|b| values::decode(b)).transpose()?;
                let key = SameValue::of(value.as_ref());
                let raw = bytes.unwrap_or_else(|| values::encode(&Value::Nil));
                match groups.iter_mut().find(|(k, _, _)| !key.is_unique() && *k == key) {
                    Some((_, _, members)) => members.push(id),
                    None => groups.push((key, raw, vec![id])),
                }
            }
            w.map(3);
            w.kind("groups");
            w.str("groups");
            let start = w.0.len();
            w.array(groups.len() as u32);
            for (_, raw, members) in &groups {
                w.array(2);
                w.raw(raw);
                if *counts {
                    w.uint(members.len() as u64);
                    continue;
                }
                w.array(members.len() as u32);
                for id in members {
                    w.str(id);
                }
            }
            let plain = !may_hold_proto(&w.0[start..]);
            w.str("plainObjects");
            w.bool(plain);
        }
        Terminal::Hits { fields, highlights } => {
            let searches = |step: &Step| {
                matches!(step, Step::Search { .. } | Step::Near { .. } | Step::Like { .. } | Step::Hybrid { .. })
            };
            if !plan.steps.iter().any(searches) {
                return Err(Error::new(ErrorCode::BatchInvalid, "hits() answers a search, and the plan has none"));
            }
            let ids = q.collect(last)?;
            let hits = q.hits.borrow();
            w.map(3);
            w.kind("hits");
            w.str("hits");
            let start = w.0.len();
            w.array(ids.len() as u32);
            for id in &ids {
                let hit = hits.get(id);
                // A vector search matches no terms, so its hits have nothing to highlight
                let marks = match (hit, *highlights) {
                    (Some((hit, name)), true) if !hit.terms.is_empty() => {
                        let index = text::resolve(r.schema(), ids::type_of(id), Some(name), "hits")?;
                        Some(text::search::highlights(r, &index, hit)?)
                    }
                    (_, true) => Some(Vec::new()),
                    _ => None,
                };
                w.map(3 + u32::from(marks.is_some()) + u32::from(fields.is_some()));
                w.str("id");
                w.str(id);
                w.str("score");
                rmp::encode::write_f64(w.0, hit.map(|(h, _)| h.score).unwrap_or(0.0)).expect("a Vec");
                w.str("terms");
                let terms = hit.map(|(h, _)| h.terms.as_slice()).unwrap_or_default();
                w.array(terms.len() as u32);
                for term in terms {
                    w.str(term);
                }
                if let Some(marks) = marks {
                    w.str("highlights");
                    w.map(marks.len() as u32);
                    for (field, spans) in &marks {
                        w.str(field);
                        w.array(spans.len() as u32);
                        for span in spans {
                            w.array(3);
                            for n in span {
                                w.uint(u64::from(*n));
                            }
                        }
                    }
                }
                if let Some(names) = fields {
                    w.str("row");
                    write_row(r, id, Some(names), &mut w)?;
                }
            }
            let plain = !may_hold_proto(&w.0[start..]);
            w.str("plainObjects");
            w.bool(plain);
        }
    }
    visited.entities += q.visited.get();
    Ok(())
}

/// A type's entities in the scope's ranges, with their places, in creation order
fn type_entities(r: &impl Read, entity_type: &str, scope: Scope) -> Result<Vec<(u64, String)>> {
    let mut out = Vec::new();
    let ranges: &[bool] = match scope {
        Scope::Live => &[false],
        Scope::Trashed => &[true],
        Scope::WithTrashed => &[false, true],
    };
    for &trashed in ranges {
        for (key, id) in r.scan(&keys::type_range(trashed, entity_type))? {
            let seq = match decode_key(&key)?.pop() {
                Some(Part::Number(n)) => n as u64,
                _ => 0,
            };
            out.push((seq, values::decode(&id)?.as_str().unwrap_or_default().to_owned()));
        }
    }
    out.sort_by_key(|(seq, _)| *seq);
    Ok(out)
}

/// The labels matching a quick-open's query, best first, ties in creation order
fn find_labels(
    r: &impl Read,
    query: &str,
    fields: &std::collections::BTreeMap<String, String>,
    limit: u32,
    scope: Scope,
    w: &mut W<'_>,
) -> Result<()> {
    let mut found: Vec<(f64, u64, String, Vec<u32>)> = Vec::new();
    for (entity_type, name) in fields {
        if !r.schema().types.contains_key(entity_type) {
            return Err(Error::new(
                ErrorCode::UnknownType,
                format!("findLabels: \"{entity_type}\" isn't a type the schema declares"),
            ));
        }
        for (seq, id) in type_entities(r, entity_type, scope)? {
            let Some(label) = entities::get(r, &id, name)? else { continue };
            let Some(label) = label.as_str() else { continue };
            if let Some(m) = text::labels::score(query, label) {
                found.push((m.score, seq, id, m.positions));
            }
        }
    }
    found.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal).then_with(|| a.1.cmp(&b.1)));
    found.truncate(limit as usize);
    w.map(2);
    w.kind("labels");
    w.str("hits");
    w.array(found.len() as u32);
    for (score, _, id, positions) in &found {
        w.map(3);
        w.str("id");
        w.str(id);
        w.str("score");
        rmp::encode::write_f64(w.0, *score).expect("a Vec");
        w.str("positions");
        w.array(positions.len() as u32);
        for p in positions {
            w.uint(u64::from(*p));
        }
    }
    Ok(())
}
