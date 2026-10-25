//! Running reads (`plan.rs`) against a snapshot or a unit's transaction, and writing their answers as one msgpack
//! envelope. A query pulls ids through its steps one at a time, so `first`, `exists`, `count` and `limit` stop once they
//! have their answer; a step needing the whole set (`orderBy`, `reverse`, `distinct`, the deep walks) collects it.
//! A `where` or `withRole` straight after a type start reads a value index where one covers the field.

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::HashSet;

use rmpv::Value;

use crate::error::{Error, ErrorCode, Result};
use crate::format::key::{Part, decode as decode_key};
use crate::format::value::TIMESTAMP_EXTENSION;

use super::entities::{self, field};
use super::keys::{self, ROLE_INDEX, SHORT_CODE_INDEX};
use super::plan::{Plan, ReadOp, Scope, Start, Step, Terminal};
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
    answer(r, &ReadOp::Query { plan: plan.clone(), visits: false }, &mut out, &mut visited)?;
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
        ReadOp::Query { plan, visits: false } => query(r, plan, out, visited),
        ReadOp::Query { plan, visits: true } => {
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
        ReadOp::WouldCreateCycle { source, target, kinds } => {
            for kind in kinds {
                relations::definition(r, kind)?;
            }
            let cycle = would_create_cycle(r, source, target, kinds)?;
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

/// A field's stored bytes; `entityType` is read from the id of an entity that exists
fn get(r: &impl Read, id: &str, name: &str) -> Result<Option<Vec<u8>>> {
    if name == field::ENTITY_TYPE {
        return Ok(entities::record(r, id)?.map(|_| values::encode(&ids::type_of(id).into())));
    }
    r.get(&keys::value(id, name))
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

/// Whether `target` already reaches `source` through the kinds, over the entities a live query reaches
fn would_create_cycle(r: &impl Read, source: &str, target: &str, kinds: &[String]) -> Result<bool> {
    let mut seen = HashSet::new();
    let mut stack = vec![target.to_owned()];
    while let Some(node) = stack.pop() {
        if node == source {
            return Ok(true);
        }
        for kind in kinds {
            for (_, far) in relations::edges(r, &node, true, Some(kind))? {
                if is_live(r, &far)? && seen.insert(far.clone()) {
                    stack.push(far);
                }
            }
        }
    }
    Ok(false)
}

fn is_live(r: &impl Read, id: &str) -> Result<bool> {
    Ok(entities::record(r, id)?.is_some_and(|rec| !rec.trashed))
}

/// A query over one read
struct Query<'r, R: Read> {
    r: &'r R,
    plan: &'r Plan,
    visited: Cell<u64>,
}

type Visit<'a> = dyn FnMut(&str) -> Result<bool> + 'a;

impl<R: Read> Query<'_, R> {
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
            self.visited.set(self.visited.get() + 1);
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
                            self.visited.set(self.visited.get() + 1);
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
                            self.visited.set(self.visited.get() + 1);
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
                    && name != field::ENTITY_TYPE
                    && !value.is_nil()
                    && let Some(part) = values::index_part(value)
                {
                    return self.holding(types, name, &part, value, visit);
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
            match index_on(self.r, t, name) {
                Some(index) => {
                    let mut found: Vec<(u64, String)> = Vec::new();
                    for (key, id) in self.r.scan(&keys::index_range(t, &index, std::slice::from_ref(part)))? {
                        let seq = match decode_key(&key)?.pop() {
                            Some(Part::Number(n)) => n as u64,
                            _ => 0,
                        };
                        found.push((seq, values::decode(&id)?.as_str().unwrap_or_default().to_owned()));
                    }
                    found.sort_by_key(|(seq, _)| *seq);
                    found.dedup_by(|a, b| a.1 == b.1);
                    for (_, id) in found {
                        self.visited.set(self.visited.get() + 1);
                        if self.admits(&id)? && !visit(&id)? {
                            return Ok(false);
                        }
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
            crate::schema::IndexDef::Value { fields, .. } if fields.first().is_some_and(|f| f == name) => {
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
        Some(Value::Ext(t, data)) if *t == TIMESTAMP_EXTENSION => SortKey::Time(timestamp_ms(data)),
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

/// A msgpack timestamp's milliseconds: 32 bits of seconds, 64 bits of nanoseconds and seconds, or 96 bits
fn timestamp_ms(data: &[u8]) -> f64 {
    match data.len() {
        4 => f64::from(u32::from_be_bytes(data.try_into().unwrap())) * 1000.0,
        8 => {
            let v = u64::from_be_bytes(data.try_into().unwrap());
            (v & 0x3_ffff_ffff) as f64 * 1000.0 + (v >> 34) as f64 / 1e6
        }
        12 => {
            let nanos = u32::from_be_bytes(data[..4].try_into().unwrap());
            let secs = i64::from_be_bytes(data[4..].try_into().unwrap());
            secs as f64 * 1000.0 + f64::from(nanos) / 1e6
        }
        _ => 0.0,
    }
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
        other => match values::as_number(other) {
            Some(n) if n.fract() == 0.0 && n.abs() < 1e21 => format!("{}", n as i64),
            Some(n) => format!("{n}"),
            None => "null".to_owned(),
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
    let q = Query { r, plan, visited: Cell::new(0) };
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
        Terminal::GroupBy { field: name } => {
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
                w.array(members.len() as u32);
                for id in members {
                    w.str(id);
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
