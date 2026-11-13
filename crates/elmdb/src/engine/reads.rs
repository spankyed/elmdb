//! Running reads (`plan.rs`) against a snapshot or a unit's transaction, and writing their answers as one msgpack
//! envelope. A query pulls ids through its steps one at a time, so `first`, `exists`, `count` and `limit` stop once they
//! have their answer; a step needing the whole set (`orderBy`, `reverse`, `distinct`, the deep walks) collects it.
//! A type start's leading filters are read as the places their value indexes hold in common (`Query::filtered`), the
//! rest tested against each; `orderBy` followed by `limit` holds only the first `limit`.

use std::cell::{Cell, OnceCell, RefCell};
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};

use rmpv::Value;
use roaring::RoaringTreemap;

use crate::error::{Error, ErrorCode, Result};
use crate::format::Prefix;
use crate::format::key::{Part, decode as decode_key, prefix_range};
use crate::format::value::TIMESTAMP_EXTENSION;

use super::algorithms::{self, Groups, Projection};
use super::entities::{self, field};
use super::keys::{self, ROLE_INDEX, SHORT_CODE_INDEX};
use super::plan::{Direction, Plan, ReadOp, Scope, SearchOptions, Similarity, Start, Step, Terminal, WhileBuilding};
use super::text::search::{Hit, Matches, RawClause};
use super::text::{self, TextIndex};
use super::trigram::{self, Matcher};
use super::unit::Read;
use super::{ids, ranks, relations, values};

/// What one plan's run read, for the specs that show a query stops early
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Visited {
    pub entities: u64,
    /// The places of lists and of rows its leading filters were handed, each one walked or sought
    pub places: u64,
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
                Some(stored) => write_relation(r, id, &stored, &mut w),
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
                write_relation(r, id, stored, &mut w);
            }
            Ok(())
        }
        ReadOp::Suggest { entity_type, index, term, limit } => {
            let found = text::resolve_built(r, entity_type, index.as_deref(), "suggest")?;
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
        ReadOp::Catalog => catalog(r, out),
        ReadOp::WouldCreateCycle { source, target, kinds } => {
            for kind in kinds {
                relations::definition(r, kind)?;
            }
            let cycle = relations::would_cycle(r, source, target, kinds, None)?;
            let mut w = W(out);
            w.map(2);
            w.kind("bool");
            w.str("value");
            w.bool(cycle);
            Ok(())
        }
    }
}

/// A relation's row as a read answers it, with its `weight` when its kind is declared weighted: the row's, or 1
fn write_relation(r: &impl Read, id: &str, s: &relations::Stored, w: &mut W<'_>) {
    let weighted = relations::definition(r, &s.kind).is_ok_and(|def| def.weighted);
    w.map(if weighted { 6 } else { 5 });
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
    if weighted {
        w.str("weight");
        rmp::encode::write_f64(w.0, s.weight.unwrap_or(1.0)).expect("a Vec");
    }
}

/// A field's msgpack, decompressed where it is stored compressed; `entityType` is read from the id of an entity that exists
/// `ReadOp::Catalog`'s answer
fn catalog(r: &impl Read, out: &mut Vec<u8>) -> Result<()> {
    let schema = r.schema();
    let mut w = W(out);
    w.map(4);
    w.kind("catalog");
    w.str("types");
    w.array(schema.types.len() as u32);
    for (name, def) in &schema.types {
        let mut fields: BTreeSet<String> = def.fields.keys().cloned().collect();
        for (key, _) in r.scan(&keys::field_names(name))? {
            if let Some(Part::String(field)) = decode_key(&key)?.pop() {
                fields.insert(field);
            }
        }
        w.map(3);
        w.str("name");
        w.str(name);
        w.str("count");
        w.uint(entities::count(r, name)?);
        w.str("fields");
        w.array(fields.len() as u32);
        for field in &fields {
            w.str(field);
        }
    }
    w.str("relations");
    w.array(schema.relations.len() as u32);
    for kind in schema.relations.keys() {
        w.map(2);
        w.str("kind");
        w.str(kind);
        w.str("count");
        w.uint(relations::count(r, kind)?);
    }
    w.str("roles");
    w.array(schema.roles.len() as u32);
    for role in schema.roles.keys() {
        w.str(role);
    }
    Ok(())
}

fn get(r: &impl Read, id: &str, name: &str) -> Result<Option<Vec<u8>>> {
    if name == field::ENTITY_TYPE {
        return Ok(entities::record(r, id)?.map(|_| values::encode(&ids::type_of(id).into())));
    }
    r.get(&keys::value(id, name))?.map(values::unstored_owned).transpose()
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

/// Where a `ranked` read takes each entity's score from
enum RankScores {
    /// The index, its nodes' estimates as a share of these totals
    Held(ranks::Totals),
    /// `rank` over its graph, while it isn't built
    Computed(HashMap<String, f64, Fixed>),
}

/// The most rounds `rank` runs for an index not built yet, which its tolerance stops long before
const COMPUTED_RANK_ROUNDS: u32 = 1_000_000;

/// How many places the value keys between two hold, or how many rows there are, as far as a read has counted,
/// and the key its count of the rest goes on from
type Counted = (u64, Vec<u8>);

/// One list of a type's places in creation order: what a value index files under the value a filter asks of, the
/// type's trash, or what a range's index held, read and put in that order (`Query::filtered`). The entity at a
/// place is the one its type's rows name there
enum Places {
    /// The places a value index files under the key `own` (`engine/places.rs`), read a key of them at a time
    Filed { own: Vec<u8>, sought: text::postings::Sought },
    /// A type's rows whose key starts with `under`, which end before `to`: each key ends with its entity's place
    Rows { under: Vec<u8>, to: Vec<u8> },
    /// Places read from the store and sorted, and how many of them a seek has left behind
    Held { places: Vec<u64>, passed: usize },
}

impl Places {
    fn filed(own: Vec<u8>) -> Self {
        let sought = text::postings::Sought::new(super::places::term(own.clone()));
        Places::Filed { own, sought }
    }

    fn rows(under: Vec<u8>) -> Self {
        let (_, to) = prefix_range(&under);
        Places::Rows { under, to }
    }

    /// The first place the list holds that is `place` or after it. Places read and sorted are sought at places
    /// that only grow
    fn seek(&mut self, r: &impl Read, place: u64) -> Result<Option<u64>> {
        match self {
            Places::Filed { sought, .. } => sought.seek(r, place, &mut false),
            Places::Rows { under, to } => {
                let mut found = None;
                r.visit_within(&crate::format::key::with_number(under, place as f64), to, |entry, _| {
                    found = Some(place_of(entry));
                    false
                })?;
                found.transpose()
            }
            Places::Held { places, passed } => {
                *passed += places[*passed..].partition_point(|held| *held < place);
                Ok(places.get(*passed).copied())
            }
        }
    }

    /// Hands `each` the list's places from `place` on, in order, until it answers false: the store is read as the
    /// walk comes to it, and no further than it is taken. False when `each` stopped it
    fn from(&mut self, r: &impl Read, place: u64, each: &mut dyn FnMut(u64) -> Result<bool>) -> Result<bool> {
        match self {
            Places::Filed { sought, .. } => super::places::from(r, sought, place, each),
            Places::Rows { under, to } => walk(
                |entry| r.visit_within(&crate::format::key::with_number(under, place as f64), to, entry),
                |entry, _| each(place_of(entry)?),
            ),
            Places::Held { places, .. } => {
                let first = places.partition_point(|held| *held < place);
                for held in &places[first..] {
                    if !each(*held)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }
}

/// The place a row of a type is at: the number its key ends with
fn place_of(entry: &[u8]) -> Result<u64> {
    crate::format::key::last_number(entry)
        .map(|place| place as u64)
        .ok_or_else(|| Error::new(ErrorCode::Corrupted, "A type's row ends with no place: rebuild() derives it again"))
}

/// How far apart the places of a list have been, where a read saw them: what says which of several lists holds the
/// fewest places, and so is the one to walk (`Query::common`)
#[derive(Clone, Copy, Default)]
struct Spread {
    /// The last place the list was seen to hold
    last: Option<u64>,
    /// How many places it was seen to hold, one after another, and how many places lie between them all: from where
    /// the read began to the first, and from each to the next
    seen: u64,
    apart: u64,
}

impl Spread {
    /// Notes that the list holds `place`, the first it was seen to hold from `from` on or one after the last
    fn saw(&mut self, from: u64, place: u64) {
        let since = self.last.unwrap_or(from);
        if self.last.is_none() || place > since {
            self.seen += 1;
            self.apart += place - since;
            self.last = Some(place);
        }
    }

    /// Whether this list's places have been more than `times` as far apart as `other`'s
    fn wider(&self, times: u128, other: &Spread) -> bool {
        u128::from(self.apart) * u128::from(other.seen) > times * u128::from(other.apart) * u128::from(self.seen)
    }
}

/// A list a type's leading filters are read from (`Query::filtered`), the index it is of, and the steps it stands
/// for: an entity it holds is one each of them keeps, so none of them is tested against it
struct Listed {
    places: Places,
    index: Option<String>,
    covers: Vec<usize>,
    spread: Spread,
    /// The place last sought in the store, and what was found: the answer to any place between the two
    last: Option<(u64, Option<u64>)>,
}

impl Listed {
    fn new(places: Places, index: Option<String>, covers: Vec<usize>) -> Self {
        Listed { places, index, covers, spread: Spread::default(), last: None }
    }

    /// The first place the list holds that is `place` or after it. The list is sought, and `read` grows by one,
    /// unless the last place sought was no later and what it found no earlier: nothing lies between those two
    fn ask(&mut self, r: &impl Read, place: u64, read: &mut u64) -> Result<Option<u64>> {
        if let Some((sought, found)) = self.last
            && sought <= place
            && found.is_none_or(|found| place <= found)
        {
            return Ok(found);
        }
        let found = self.places.seek(r, place)?;
        *read += 1;
        self.last = Some((place, found));
        if let Some(found) = found {
            self.spread.saw(place, found);
        }
        Ok(found)
    }
}

/// The entries of an index that a `range` of numbers among a type's leading filters is read from, between two keys,
/// and the steps they stand for (`Query::ranges_of`): the range, and with it the equalities an index on their fields
/// and then the range's holds it under. Their order is the numbers', so they are a list of places only once read
struct Ranged {
    from: Vec<u8>,
    to: Vec<u8>,
    index: String,
    covers: Vec<usize>,
}

/// What a read of the places some lists hold in common does with each (`Query::common`)
struct Asks<'a> {
    /// The type the places are of
    entity_type: &'a str,
    /// The steps of the filters tested against the entity at each
    tested: &'a [usize],
    /// How many entries the read may have read before it pauses
    pause_at: u64,
    /// Where the places are kept, for a read that takes them as a set: no entity is read at one, and none is
    /// handed on
    kept: Option<&'a RefCell<RoaringTreemap>>,
}

/// How a read of the places some lists hold in common left off (`Query::common`)
enum Left {
    /// What it handed a place to wanted no more
    Stopped,
    /// A list ran out: no place after is in them all
    Ended,
    /// It had read as much as it was to before being asked what else there is to read
    Paused,
}

/// The keys a cost question counts under: those that are the first of these or after it, and before the second
type Entries = (Vec<u8>, Vec<u8>);

struct Query<'r, R: Read> {
    r: &'r R,
    plan: &'r Plan,
    visited: Cell<u64>,
    /// The places the leading filters were handed (`Visited::places`)
    places: Cell<u64>,
    /// What each stage did, for a read asked to explain itself
    stages: Option<&'r Stages>,
    /// What the plan's search found in each entity, with the index it searched, for the `hits` terminal
    hits: RefCell<HashMap<String, (Hit, String), Fixed>>,
    /// The stage of a hop a `reverse` straight after it asks to walk its edges from the last (`Step::Reverse`)
    backward_hop: Cell<Option<usize>>,
    /// The stage of a hop that did, handing its ids on already reversed
    walked_back: Cell<Option<usize>>,
    /// What the stage about to run is asked for, when less than all it would hand on will do: set by a `limit` or a
    /// terminal as it pulls a search (`Query::asks`), and taken by whichever stage runs next
    asked: Cell<Option<Ask>>,
    /// The stage that started past the cursor of the `after` it hands to (`Step::After`), handing on only the ids
    /// past it
    sought: Cell<Option<usize>>,
    /// What the plan's last rank or ranked step says of its scores, for the `hits` terminal
    ranked: RefCell<Option<Scored>>,
    /// The ids each `within` of the plan keeps, by the step's place, made when one is first asked for (`kept_within`)
    within: Vec<OnceCell<HashSet<&'r str>>>,
    /// The bounds of each `range` of the plan, by the step's place, read when first asked for (`bounds_at`)
    bounds: Vec<OnceCell<Bounds>>,
    /// The keys between each two keys that a cost question has counted, this far (`counted_within`)
    counted: RefCell<HashMap<Entries, Counted>>,
    /// How many filters the plan starts with (`leading_filters`)
    filters: usize,
    /// Whether an index reads any of those filters in any start type, asked once a read (`reads_together`)
    together: OnceCell<bool>,
}

impl<'r, R: Read> Query<'r, R> {
    /// The ids the `within` at step `at` keeps, as a set made once a read: a filter tested against each entity an
    /// index hands on (`passes`) costs a lookup, where looking through the step's ids costs their number each time
    fn kept_within(&self, at: usize) -> &HashSet<&'r str> {
        self.within[at].get_or_init(|| {
            let plan: &'r Plan = self.plan;
            match &plan.steps[at] {
                Step::Within { ids } => ids.iter().map(String::as_str).collect(),
                _ => HashSet::new(),
            }
        })
    }
}

/// What a `hits` answer says of a rank's scores
enum Scored {
    /// A `rank`: whether it settled within its tolerance, and in how many rounds
    Rank { converged: bool, iterations: u32 },
    /// A `ranked`: the index read, and the bound on its scores' distance from PageRank
    Ranked { index: String, error: f64 },
}

type Visit<'a> = dyn FnMut(&str) -> Result<bool> + 'a;

/// What the places an index files are handed to as they are read, each an entity's: false to stop the read
type Entry<'a> = dyn FnMut(u64) -> Result<bool> + 'a;

/// A search's match as it is ranked: its score; what breaks a tie, its place in the set searched among, or its
/// type's among the types searched and then its own in creation order; and which of the indexes searched it is of
#[derive(PartialEq)]
struct Ranked {
    score: f64,
    first: u64,
    seq: u64,
    slot: usize,
}

impl Ord for Ranked {
    /// The better match is the greater: a higher score, then an earlier place
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.first.cmp(&self.first))
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// No score is NaN, a sum of products of numbers that aren't, so a match is its own equal as its order needs
impl Eq for Ranked {}

/// What a stage is asked for by the one pulling it, when less than all it would hand on will do. A search answers
/// it; any other stage hands on what it would have
#[derive(Clone, Copy)]
enum Ask {
    /// No more than the best so many of its matches: a `limit`, or a terminal that answers the first
    Best(usize),
    /// One of its matches, whichever: an `exists`, which answers whether there is one
    Any,
}

/// The matches a search hands on: every one, or no more than the best `take` of them, kept as they are scored
enum Best {
    Every(Vec<Ranked>),
    First(usize, BinaryHeap<Reverse<Ranked>>),
}

impl Best {
    fn new(take: Option<usize>) -> Self {
        match take {
            Some(take) => Best::First(take, BinaryHeap::new()),
            None => Best::Every(Vec::new()),
        }
    }

    #[inline]
    fn keep(&mut self, ranked: Ranked) {
        match self {
            Best::Every(all) => all.push(ranked),
            Best::First(take, kept) => {
                if kept.len() < *take {
                    return kept.push(Reverse(ranked));
                }
                // mutant-skip: replace > with >= in Best::keep
                // No match is another's equal: each has a place among them of its own
                if kept.peek().is_some_and(|Reverse(least)| ranked > *least) {
                    kept.pop();
                    kept.push(Reverse(ranked));
                }
            }
        }
    }

    /// How many matches are kept
    fn len(&self) -> usize {
        match self {
            Best::Every(all) => all.len(),
            Best::First(_, kept) => kept.len(),
        }
    }

    /// The matches kept, the best the first out
    fn into_heap(self) -> BinaryHeap<Ranked> {
        match self {
            Best::Every(all) => BinaryHeap::from(all),
            Best::First(_, kept) => kept.into_iter().map(|Reverse(ranked)| ranked).collect(),
        }
    }
}

/// FxHash, for the maps the graph algorithms build, which hash an id per edge: its keys are fixed, where one keyed per
/// process would make a read's work differ from run to run, which the instruction counts compare
/// (`scripts/diagnostics-overhead.py`); and it is several times cheaper than std's SipHash on ids this short. The keys
/// are ids from the store, which the process reading them wrote or was given, so no one picks them to collide
type Fixed = rustc_hash::FxBuildHasher;

impl<R: Read> Query<'_, R> {
    /// The leading filters were handed this many places more, each walked or sought: counted for the plan, and for
    /// the stage running when the read is explained
    fn handed(&self, places: u64) {
        self.places.set(self.places.get() + places);
        if let Some(stages) = self.stages {
            stages.handed(places);
        }
    }

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
        Ok(self.admitted(id)?.is_some())
    }

    /// The entity's place in creation order when the start may begin from it
    fn admitted(&self, id: &str) -> Result<Option<u64>> {
        Ok(entities::record(self.r, id)?
            .filter(|rec| match self.plan.scope {
                Scope::Live => !rec.trashed,
                Scope::WithTrashed => true,
                Scope::Trashed => rec.trashed,
            })
            .map(|rec| rec.seq))
    }

    /// Visits a type's entities in creation order, in the scope's ranges, all of them or those past the place `after`,
    /// which the walk seeks to; false when `visit` stopped it. A walk goes through its range as it visits, so one
    /// stopped early reads no further; a walk of both ranges collects the trash, which is the smaller, and merges it
    /// into the live range by place
    fn walk_type(&self, entity_type: &str, after: Option<u64>, visit: &mut Visit<'_>) -> Result<bool> {
        self.walk_places(entity_type, after, &mut |_, id| visit(id))
    }

    /// `walk_type`, handing on each entity's place with its id
    fn walk_places(
        &self,
        entity_type: &str,
        after: Option<u64>,
        visit: &mut dyn FnMut(u64, &str) -> Result<bool>,
    ) -> Result<bool> {
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
                let trashed = entry(&key, &id)?;
                if after.is_none_or(|after| trashed.0 > after) {
                    trash.push_back(trashed);
                }
            }
        }
        let mut go = |seq: u64, id: &str| -> Result<bool> {
            self.count_visit();
            visit(seq, id)
        };
        let mut outcome: Result<bool> = Ok(true);
        let trashed = self.plan.scope == Scope::Trashed;
        let range = keys::type_range(trashed, entity_type);
        // Past the place, or from the range's first entry, which no key is before
        let from = after.map(|after| past(keys::of_type(trashed, entity_type, after))).unwrap_or_default();
        self.r.visit_from(&range, &from, |key, id| {
            let step = (|| -> Result<bool> {
                let (seq, id) = entry(key, id)?;
                while trash.front().is_some_and(|(t, _)| *t < seq) {
                    let (place, trashed) = trash.pop_front().expect("a front entry");
                    if !go(place, &trashed)? {
                        return Ok(false);
                    }
                }
                go(seq, &id)
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
        while let Some((place, trashed)) = trash.pop_front() {
            if !go(place, &trashed)? {
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

    /// Whether the set after the first `before` steps holds each entity once for certain: a start of distinct types,
    /// nothing after it. Any other set may repeat one
    fn holds_each_once(&self, before: usize) -> bool {
        before == 0
            && self.start_types().is_some_and(|types| types.iter().enumerate().all(|(at, t)| !types[..at].contains(t)))
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
        // What the stage pulling this one asked of it, which only a search answers
        let ask = self.asked.take();
        // The cursor of the `after` this stage hands to, the step after it, which alone pulls it: a stage that reads
        // an order it can seek in starts past the cursor
        let cursor = match self.plan.steps.get(upto) {
            Some(Step::After { id }) => Some(id.as_str()),
            _ => None,
        };
        if upto == 0 {
            return match &self.plan.start {
                Start::Types { types } => {
                    let from = match cursor {
                        Some(cursor) => {
                            self.sought.set(Some(upto));
                            match self.cursor_among(types, cursor)? {
                                Some(place) => Some(place),
                                None => return Ok(true),
                            }
                        }
                        None => None,
                    };
                    for (at, t) in types.iter().enumerate() {
                        let Some(after) = starts_past(from, at) else { continue };
                        if !self.walk_type(t, after, visit)? {
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
        // The filters a type start begins with are read together, by the stage of the last of them, where an
        // index reads any of them. With none, the stage of the first walks the types for it, and each stage after
        // tests its own filter against what the one before hands on
        if let Some(types) = self.start_types()
            && upto <= self.filters
            && upto == if self.reads_together(types)? { self.filters } else { 1 }
        {
            return self.filtered(upto, types, cursor, visit);
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
                self.pull(before, &mut |id| if self.matches(id, name, value)? { visit(id) } else { Ok(true) })
            }
            Step::Range { field: name, .. } => {
                let bounds = self.bounds_at(before)?;
                self.pull(before, &mut |id| if self.within(id, name, bounds)? { visit(id) } else { Ok(true) })
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
            Step::Both { kinds, types } => {
                for kind in kinds {
                    relations::definition(self.r, kind)?;
                }
                let mut reached = HashSet::new();
                self.pull(before, &mut |from| {
                    for kind in kinds {
                        for outward in [true, false] {
                            if !self.walk_edges(from, kind, types.as_deref(), outward, None, &mut reached, visit)? {
                                return Ok(false);
                            }
                        }
                    }
                    Ok(true)
                })
            }
            Step::Descendants { kinds, types, depth } => {
                self.reach(before, kinds, types.as_deref(), true, *depth, visit)
            }
            Step::Ancestors { kinds, types, depth } => {
                self.reach(before, kinds, types.as_deref(), false, *depth, visit)
            }
            Step::PathTo { to, kinds, direction, depth } => self.path_to(before, to, kinds, *direction, *depth, visit),
            Step::Rank { kinds, direction, seeds, weights, damping, iterations, tolerance } => {
                // NaN is in none of these ranges
                let valid = (0.0..1.0).contains(damping) && *damping > 0.0 && *tolerance > 0.0;
                if !valid || *iterations == 0 {
                    return Err(Error::new(
                        ErrorCode::BatchInvalid,
                        format!(
                            "rank takes a damping between 0 and 1, at least one iteration and a tolerance above 0, and \
                             was given {damping}, {iterations} and {tolerance}"
                        ),
                    ));
                }
                if let Some(bad) =
                    seeds.iter().flatten().find(|seed| !(seed.weight() > 0.0 && seed.weight() <= relations::MAX_WEIGHT))
                {
                    return Err(Error::new(
                        ErrorCode::BatchInvalid,
                        format!(
                            "rank takes a seed's weight above 0 and at most 2^960, and was given {} for {:?}",
                            bad.weight(),
                            bad.id()
                        ),
                    ));
                }
                let (ids, graph) = self.project(before, upto, kinds, *direction, *weights)?;
                let seeds: Vec<(u32, f64)> = match seeds {
                    None => (0..ids.len() as u32).map(|at| (at, 1.0)).collect(),
                    // The few seeds looked up as the set is walked, rather than a map of the whole set built for them;
                    // a seed named twice adds its weights
                    Some(seeds) => {
                        let mut wanted: HashMap<&str, f64, Fixed> = HashMap::default();
                        for seed in seeds {
                            wanted.entry(seed.id()).and_modify(|held| *held += seed.weight()).or_insert(seed.weight());
                        }
                        (0..ids.len() as u32)
                            .filter_map(|at| wanted.get(ids[at as usize].as_str()).map(|&weight| (at, weight)))
                            .collect()
                    }
                };
                // No seed in the set answers nothing, settled in no rounds
                let Some(ranked) = algorithms::rank(&graph, &seeds, *damping, *iterations, *tolerance) else {
                    self.ranked.replace(Some(Scored::Rank { converged: true, iterations: 0 }));
                    return Ok(true);
                };
                self.ranked.replace(Some(Scored::Rank { converged: ranked.converged, iterations: ranked.iterations }));
                if let Some(stages) = self.stages {
                    stages.ranked(upto, ranked.iterations, ranked.converged);
                }
                let mut order: Vec<usize> = (0..ids.len()).collect();
                // Stable: equal scores keep the set's order
                order.sort_by(|&a, &b| ranked.scores[b].total_cmp(&ranked.scores[a]));
                // Each hit recorded as its entity is handed on, so a limit after the rank keeps that many and no more,
                // and none for a plan whose terminal doesn't answer them
                let scored = matches!(self.plan.terminal, Terminal::Hits { .. });
                for at in order {
                    let id = &ids[at];
                    if scored {
                        let hit = Hit {
                            id: id.clone(),
                            seq: 0,
                            score: ranked.scores[at],
                            terms: Vec::new(),
                            fields: Vec::new(),
                        };
                        self.hits.borrow_mut().insert(id.clone(), (hit, "rank".to_owned()));
                    }
                    if !visit(id)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Step::Ranked { index, while_building } => self.ranked_by(before, upto, index, *while_building, visit),
            Step::Topological { kinds, direction } => {
                if *direction == Direction::Both {
                    return Err(Error::new(
                        ErrorCode::BatchInvalid,
                        "topological follows each edge one way, out or in: an edge followed both ways orders nothing",
                    ));
                }
                let (ids, graph) = self.project(before, upto, kinds, *direction, false)?;
                let ordered = algorithms::topological_order(&graph);
                if let Some(stages) = self.stages {
                    stages.cycled(upto, ordered.cycles);
                }
                for at in ordered.order {
                    if !visit(&ids[at as usize])? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Step::Similar { to, kinds, direction, measure } => {
                for kind in kinds {
                    relations::definition(self.r, kind)?;
                }
                if !self.alive(to)? {
                    return Ok(true);
                }
                // Whether an entity is in the set, and its place there. Straight after a type start that is its type and
                // the scope, read per candidate: the type's place in the start, then its creation, the order the start
                // walks. After any other step the set is collected, and each entity has its first place in it
                let types = if before == 0 { self.start_types() } else { None };
                let ids = if types.is_some() { Vec::new() } else { self.collect(before)? };
                let mut first: HashMap<&str, usize, Fixed> = HashMap::default();
                for (at, id) in ids.iter().enumerate() {
                    first.entry(id.as_str()).or_insert(at);
                }
                let place = |id: &str| -> Result<Option<(usize, u64)>> {
                    match types {
                        Some(types) => {
                            let Some(at) = types.iter().position(|t| t == ids::type_of(id)) else { return Ok(None) };
                            Ok(self.admitted(id)?.map(|seq| (at, seq)))
                        }
                        None => Ok(first.get(id).map(|&at| (at, 0))),
                    }
                };
                let back: &[bool] = match direction {
                    Direction::Out => &[false],
                    Direction::In => &[true],
                    Direction::Both => &[true, false],
                };
                let mut walked = 0usize;
                let mut ours = Vec::new();
                self.visit_neighbours(to, kinds, direction.sides(), &mut walked, |far| {
                    ours.push(far.to_owned());
                    Ok(())
                })?;
                // Each entity of the set that shares a neighbour with `to`: its place, how many it shares, and their
                // Adamic-Adar sum, added in the order `to`'s neighbours were read so the sum is the same bits every time.
                // An entity found outside the set is remembered, so it is looked up once
                let mut shared: HashMap<String, ((usize, u64), usize, f64), Fixed> = HashMap::default();
                let mut outside: HashSet<String, Fixed> = HashSet::default();
                for neighbour in &ours {
                    let mut degree = 0usize;
                    let mut found = Vec::new();
                    self.visit_neighbours(neighbour, kinds, back, &mut walked, |far| {
                        degree += 1;
                        if far != to.as_str() && !outside.contains(far) {
                            found.push(far.to_owned());
                        }
                        Ok(())
                    })?;
                    let term = algorithms::adamic_adar(degree);
                    for other in found {
                        if let Some(entry) = shared.get_mut(&other) {
                            entry.1 += 1;
                            entry.2 += term;
                            continue;
                        }
                        match place(&other)? {
                            Some(at) => {
                                shared.insert(other, (at, 1, term));
                            }
                            None => {
                                outside.insert(other);
                            }
                        }
                    }
                }
                let mut scored = Vec::with_capacity(shared.len());
                for (id, (at, count, sum)) in shared {
                    let score = match measure {
                        Similarity::Jaccard => {
                            let mut theirs = 0usize;
                            self.visit_neighbours(&id, kinds, direction.sides(), &mut walked, |_| {
                                theirs += 1;
                                Ok(())
                            })?;
                            algorithms::jaccard(count, theirs, ours.len())
                        }
                        Similarity::AdamicAdar => sum,
                    };
                    scored.push((at, id, score));
                }
                if let Some(stages) = self.stages {
                    stages.compared(upto, ours.len(), scored.len(), walked);
                }
                // Highest first, equal scores in the set's order
                scored.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
                let record = matches!(self.plan.terminal, Terminal::Hits { .. });
                for (_, id, score) in scored {
                    if record {
                        let hit = Hit { id: id.clone(), seq: 0, score, terms: Vec::new(), fields: Vec::new() };
                        self.hits.borrow_mut().insert(id.clone(), (hit, "similar".to_owned()));
                    }
                    if !visit(&id)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Step::OrderBy { field: name, descending } => {
                if let Some(ordered) = self.ordered(before, name)? {
                    let begin = match cursor {
                        Some(cursor) => {
                            self.sought.set(Some(upto));
                            let Some(begin) = self.past_cursor(before, &ordered, name, cursor)? else {
                                return Ok(true);
                            };
                            begin
                        }
                        None => Begin::First,
                    };
                    return self.in_order(&ordered, *descending, begin, &mut |place| {
                        self.hand(ordered.entity_type, place, visit)
                    });
                }
                self.note_wanted(upto, before, name)?;
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
                // A hop straight before walks its edges from the last where it can, handing its ids on reversed, so a
                // limit after this one reads only as far as it keeps
                if before > 0 && matches!(self.plan.steps[before - 1], Step::Out { .. } | Step::In { .. }) {
                    self.backward_hop.set(Some(before));
                }
                let mut ids = Vec::new();
                let go = self.pull(before, &mut |id| {
                    if self.walked_back.get() == Some(before) {
                        visit(id)
                    } else {
                        ids.push(id.to_owned());
                        Ok(true)
                    }
                })?;
                if self.walked_back.get() == Some(before) {
                    return Ok(go);
                }
                ids.reverse();
                each(ids, visit)
            }
            Step::Limit { n } => {
                let take = if n.is_nan() { 0 } else { n.floor() as i64 };
                if take <= 0 {
                    return Ok(true);
                }
                if before == 1
                    && let Step::Ranked { index, .. } = &self.plan.steps[0]
                    && self.reads_rank_top(index)?
                {
                    let first = self.rank_top(index, take as usize)?;
                    if let Some(stages) = self.stages {
                        for _ in &first {
                            stages.kept(before);
                        }
                    }
                    return each(first, visit);
                }
                // The order this takes the first of, straight before it or before its page's cursor, with how many
                // steps the order is of
                let ordering = match &self.plan.steps[..before] {
                    [under @ .., Step::OrderBy { field, descending }] => Some((under.len(), field, *descending, None)),
                    [under @ .., Step::OrderBy { field, descending }, Step::After { id }] => {
                        Some((under.len(), field, *descending, Some(id.as_str())))
                    }
                    _ => None,
                };
                // An order read from an index comes in it, and the walk below stops it at what this takes. Any other
                // is walked while that is the cheaper way, or sorted with only what this takes held, a page's as
                // the first's
                if let Some((under, name, descending, cursor)) = ordering
                    && self.ordered(under, name)?.is_none()
                {
                    let order = under + 1;
                    self.note_wanted(order, under, name)?;
                    let walked = {
                        // What the walk reads is the order's stage's, as what the sort pulls is
                        let _order = self.stages.map(|stages| stages.enter(order));
                        self.walked_first(under, name, descending, take as usize, cursor)?
                    };
                    let first = match walked {
                        Some(first) => first,
                        None => self.first_ordered(under, name, descending, take as usize, cursor)?,
                    };
                    // The stages this read for, the order's and its cursor's, kept what they hand on
                    if let Some(stages) = self.stages {
                        for stage in order..=before {
                            for _ in &first {
                                stages.kept(stage);
                            }
                        }
                    }
                    return each(first, visit);
                }
                // A search before keeps no more of its matches than this takes
                self.asks(before, Ask::Best(take as usize));
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
                // mutant-skip: delete ! in Query<'_, R>::stage
                // A stage's answer flows only to the stages after it and the terminal, none of which reads it: an id
                // refused is what stops a walk
                Ok(!stopped)
            }
            Step::After { id: cursor } => {
                // What is asked of this stage is asked of the one before it, of what is past the cursor: only a
                // search is asked (`Query::asks`), and it seeks. Whichever one of those will do is their first
                self.asked.set(ask.map(|ask| match ask {
                    Ask::Any => Ask::Best(1),
                    best @ Ask::Best(_) => best,
                }));
                // The stage before starts past the cursor itself when it reads an order it can seek in, and says so;
                // any other hands on every id, and those up to the cursor are passed over here
                let mut past = false;
                self.pull(before, &mut |id| {
                    if past || self.sought.get() == Some(before) {
                        return visit(id);
                    }
                    past = id == cursor;
                    Ok(true)
                })
            }
            Step::Search { query, index, options } => {
                self.search(before, ask, cursor, query, index.as_deref(), options, visit)
            }
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

    /// Asks the stage after the first `upto` steps for less than all it would hand on, when it is one that answers:
    /// a search, or the `after` of one, which hands the ask on to it as one of what is past its cursor. No other
    /// stage is asked: one that handed an ask on without seeking would have it answered of the wrong ids
    fn asks(&self, upto: usize, ask: Ask) {
        if matches!(&self.plan.steps[..upto], [.., Step::Search { .. }] | [.., Step::Search { .. }, Step::After { .. }])
        {
            self.asked.set(Some(ask));
        }
    }

    /// The entities a search matches, most relevant first, ties in the order they came: a type start's whole types,
    /// or the set before the step, of whose types one with no text index holds no match. Every match is scored from
    /// the index, and an entity is read only as it is handed on, so any step stopping the walk leaves the rest
    /// unread. Asked for its best (`Query::asks`), it keeps no more than those as they are scored. Before an `after`
    /// it hands on the matches ranked past the cursor's own, which it scores first, and none when the cursor is no
    /// match of its own. Asked for any one, it scores none, and hands on its matches as the indexes hold them
    #[allow(clippy::too_many_arguments)]
    fn search(
        &self,
        before: usize,
        ask: Option<Ask>,
        cursor: Option<&str>,
        query: &str,
        index: Option<&str>,
        options: &SearchOptions,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        let raw = text::search::parse(query);
        let now = options.recency.as_ref().and_then(|r| r.now).unwrap_or_else(super::now_ms);
        // The filters a type start begins with are the set it searches within, where their indexes read them all
        let sets = self.filtered_sets(before)?;
        let (types, among) = match &sets {
            Some(sets) => (sets.iter().map(|(entity_type, _)| entity_type.clone()).collect(), None),
            None => self.searched(before)?,
        };
        let set_of = |entity_type: &str| {
            sets.as_ref().and_then(|sets| sets.iter().find(|(of, _)| of == entity_type).map(|(_, set)| set))
        };
        // Before an `after` it hands on only its matches past the cursor, and says so once the stages before it have
        // run: one of those that sought to a cursor of its own said so of itself
        if cursor.is_some() {
            self.sought.set(Some(before + 1));
        }
        let order: Option<HashMap<&str, usize>> =
            among.as_ref().map(|ids| ids.iter().enumerate().rev().map(|(at, id)| (id.as_str(), at)).collect());
        let mut indexes = Vec::with_capacity(types.len());
        for entity_type in &types {
            match text::resolve_built(self.r, entity_type, index, "search") {
                Ok(found) => indexes.push((entity_type, found)),
                // Among a set, a type with no text index holds no match; one with several, none named, is refused
                Err(_)
                    if (among.is_some() || sets.is_some())
                        && index.is_none()
                        && text::text_indexes(self.r.schema(), entity_type).is_empty() => {}
                Err(e) => return Err(e),
            }
        }
        // These scores replace a rank's before them, so the answer no longer says how that rank settled
        self.ranked.replace(None);
        // What an explained read says the stage held to rank: nothing, until it has ranked
        if let Some(stages) = self.stages {
            stages.held(0);
        }
        if let Some(Ask::Any) = ask {
            // Whichever match comes first is the answer, so none is scored and each index is read only if those
            // before it matched nothing
            for (entity_type, text_index) in &indexes {
                let (matches, ids) =
                    self.matches_in(among.as_deref(), set_of(entity_type), entity_type, text_index, &raw, options)?;
                for seq in &matches.found {
                    self.count_visit();
                    let Some(id) = self.matched_entity(entity_type, ids.as_ref(), seq)? else { continue };
                    if !visit(&id)? {
                        return Ok(false);
                    }
                }
            }
            return Ok(true);
        }
        // A page's cursor as these searches rank it: in the first index searched of its type, at its own place. One
        // of a type not searched, or that is no entity, has no page, and no index is read to say so
        let at = match cursor {
            None => None,
            Some(cursor) => {
                let Some(slot) = indexes.iter().position(|(t, _)| t.as_str() == ids::type_of(cursor)) else {
                    return Ok(true);
                };
                let Some(seq) = entities::record(self.r, cursor)?.map(|rec| rec.seq) else { return Ok(true) };
                Some((slot, seq))
            }
        };
        // Its own index is searched first: a cursor that is no match has no page, whatever the others hold
        let mut own = None;
        if let Some((slot, seq)) = at {
            let (entity_type, text_index) = &indexes[slot];
            let held =
                self.matches_in(among.as_deref(), set_of(entity_type), entity_type, text_index, &raw, options)?;
            if !held.0.found.contains(seq) {
                return Ok(true);
            }
            own = Some((slot, held));
        }
        let mut searched = Vec::with_capacity(indexes.len());
        for (slot, (entity_type, text_index)) in indexes.iter().enumerate() {
            searched.push(match own.take_if(|(of, _)| *of == slot) {
                Some((_, held)) => held,
                None => {
                    self.matches_in(among.as_deref(), set_of(entity_type), entity_type, text_index, &raw, options)?
                }
            });
        }
        // A tie is broken by the set's own order, or by the types' and then by creation's
        let first = |slot: usize, seq: u64| match (&order, &searched[slot].1) {
            (Some(order), Some(ids)) => {
                ids.get(&seq).and_then(|id| order.get(id.as_str())).map_or(u64::MAX, |at| *at as u64)
            }
            _ => slot as u64,
        };
        // The page is the matches ranked past the cursor's own score
        let last = match at {
            None => None,
            Some((slot, seq)) => {
                let Some(score) = searched[slot].0.score_of(self.r, now, seq)? else { return Ok(true) };
                Some(Ranked { score, first: first(slot, seq), seq, slot })
            }
        };
        let mut best = Best::new(match ask {
            Some(Ask::Best(take)) => Some(take),
            _ => None,
        });
        for (slot, (matches, _)) in searched.iter().enumerate() {
            matches.scored(self.r, now, |seq, score| {
                let ranked = Ranked { score, first: first(slot, seq), seq, slot };
                if last.as_ref().is_none_or(|last| ranked < *last) {
                    best.keep(ranked);
                }
            })?;
        }
        if let Some(stages) = self.stages {
            stages.held(best.len());
        }
        // Each hit recorded as its entity is handed on, and none for a plan whose terminal doesn't answer them
        let record = matches!(self.plan.terminal, Terminal::Hits { .. });
        let mut best = best.into_heap();
        while let Some(Ranked { score, seq, slot, .. }) = best.pop() {
            let (entity_type, text_index) = &indexes[slot];
            let (matches, ids) = &searched[slot];
            self.count_visit();
            let Some(id) = self.matched_entity(entity_type, ids.as_ref(), seq)? else { continue };
            if record {
                let (terms, fields) = matches.held_by(seq)?;
                let hit = Hit { id: id.clone(), seq, score, terms, fields };
                self.hits.borrow_mut().insert(id.clone(), (hit, text_index.name.clone()));
            }
            if !visit(&id)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// What a query matches in one of the indexes a search reads, with the id at each place of the set searched among
    /// that is of the index's type. From a type the matches are among the places the plan's scope holds, or those
    /// of them in `set`, and no ids are read
    fn matches_in<'a>(
        &self,
        among: Option<&[String]>,
        set: Option<&RoaringTreemap>,
        entity_type: &'a str,
        text_index: &'a TextIndex,
        raw: &[RawClause],
        options: &'a SearchOptions,
    ) -> Result<(Matches<'a>, Option<HashMap<u64, String>>)> {
        let ids = match among {
            Some(ids) => {
                let of_type: Vec<String> = ids.iter().filter(|id| ids::type_of(id) == entity_type).cloned().collect();
                Some(text::search::places(self.r, &of_type)?)
            }
            None => None,
        };
        let held;
        let (within, live) = match (&ids, set) {
            (Some(ids), _) => {
                held = Some(ids.keys().copied().collect::<RoaringTreemap>());
                (held.as_ref(), false)
            }
            // A set is of the scope's places already
            (None, Some(set)) => (Some(set), false),
            (None, None) => {
                let (places, live) = self.searched_places(entity_type)?;
                held = places;
                (held.as_ref(), live)
            }
        };
        let matches = text::search::matches(self.r, entity_type, text_index, raw, options, within, live)?;
        Ok((matches, ids))
    }

    /// The entity a match is of: the id at its place in the set searched among, or from a type the entity there
    fn matched_entity(
        &self,
        entity_type: &str,
        ids: Option<&HashMap<u64, String>>,
        seq: u64,
    ) -> Result<Option<String>> {
        match ids {
            Some(ids) => Ok(ids.get(&seq).cloned()),
            None => self.at_place(entity_type, seq),
        }
    }

    /// The places the plan's first `before` steps keep, for each of a type start's types that has any, when those
    /// steps are the filters the start begins with and an index reads every one of them in every type: the places
    /// their lists hold in common (`common`), of the entities the scope reaches. No entity is read to make them. None
    /// when a filter has no list in some type, which is then tested against each entity as the set is collected
    fn filtered_sets(&self, before: usize) -> Result<Option<Vec<(String, RoaringTreemap)>>> {
        let Some(types) = self.start_types() else { return Ok(None) };
        if before == 0 || before != self.filters {
            return Ok(None);
        }
        self.filters_taken(before)?;
        // A type named twice is searched once, as a set holds its entities once
        let mut once: Vec<&String> = Vec::new();
        for entity_type in types {
            if !once.contains(&entity_type) {
                once.push(entity_type);
            }
        }
        // Nothing is read until every type is known to have a list for every filter
        for entity_type in &once {
            if !self.lists_cover(before, entity_type)? {
                return Ok(None);
            }
        }
        let mut sets = Vec::new();
        for entity_type in once {
            let mut set = self.filtered_places(before, entity_type)?;
            if self.plan.scope != Scope::WithTrashed {
                let mut trashed = RoaringTreemap::new();
                self.r.visit(&keys::type_range(true, entity_type), |key, _| {
                    trashed.extend(keys::place_of(key));
                    true
                })?;
                if self.plan.scope == Scope::Live {
                    set -= trashed;
                } else {
                    set &= trashed;
                }
            }
            if let Some(stages) = self.stages {
                stages.ran_within(set.len());
            }
            if !set.is_empty() {
                sets.push((entity_type.clone(), set));
            }
        }
        Ok(Some(sets))
    }

    /// Whether a list of a type's places, or a range's index, reads every one of the plan's first `upto` steps:
    /// asked before any of them is read
    fn lists_cover(&self, upto: usize, entity_type: &str) -> Result<bool> {
        let lists = self.listed(upto, entity_type)?;
        let ranged = self.ranges_of(upto, entity_type)?;
        let covers = |at: &usize| {
            lists.iter().any(|list| list.covers.contains(at)) || ranged.iter().any(|range| range.covers.contains(at))
        };
        Ok((0..upto).all(|at| covers(&at)))
    }

    /// The places of a type every one of the plan's first `upto` steps keeps, read from their lists and from the
    /// ranges among them, each read whole: for a type whose lists cover every step (`lists_cover`)
    fn filtered_places(&self, upto: usize, entity_type: &str) -> Result<RoaringTreemap> {
        let mut lists = self.listed(upto, entity_type)?;
        for ranged in self.ranges_of(upto, entity_type)? {
            let list = self.read_range(ranged)?;
            lists
                .retain(|other| other.covers.is_empty() || !other.covers.iter().all(|step| list.covers.contains(step)));
            lists.push(list);
        }
        let kept = RefCell::new(RoaringTreemap::new());
        let asks = Asks { entity_type, tested: &[], pause_at: u64::MAX, kept: Some(&kept) };
        let (mut next, mut read) = (0, 0);
        if let Some(stages) = self.stages {
            for index in lists.iter().filter_map(|list| list.index.as_deref()) {
                stages.reads_index(index);
            }
            if lists.len() > 1 {
                stages.read_in_common();
            }
        }
        self.common(&mut lists, &asks, &mut next, &mut read, &mut |_| Ok(true))?;
        self.handed(read);
        Ok(kept.into_inner())
    }

    /// The places of a type a search from it reads, as those to search among and whether its trash is left out: all
    /// but the trash for a live plan, which the search leaves out of what it matched; the trash for a plan of it; and
    /// every place for one of both
    fn searched_places(&self, entity_type: &str) -> Result<(Option<RoaringTreemap>, bool)> {
        match self.plan.scope {
            Scope::Live => Ok((None, true)),
            Scope::WithTrashed => Ok((None, false)),
            Scope::Trashed => {
                let mut trashed = RoaringTreemap::new();
                self.r.visit(&keys::type_range(true, entity_type), |key, _| {
                    trashed.extend(keys::place_of(key));
                    true
                })?;
                Ok((Some(trashed), false))
            }
        }
    }

    /// The entity of a type at a place, read from the type's entities the plan's scope holds
    fn at_place(&self, entity_type: &str, seq: u64) -> Result<Option<String>> {
        let ranges: &[bool] = match self.plan.scope {
            Scope::Live => &[false],
            Scope::Trashed => &[true],
            Scope::WithTrashed => &[false, true],
        };
        for &trashed in ranges {
            if let Some(held) = self.r.get(&keys::of_type(trashed, entity_type, seq))? {
                return Ok(values::decode(&held)?.as_str().map(str::to_owned));
            }
        }
        Ok(None)
    }

    /// How many entities of the start's types a search straight after it matches, in the plan's scope: its matches
    /// counted from the index, none of them scored or read
    fn matched(&self, types: &[String], query: &str, index: Option<&str>, options: &SearchOptions) -> Result<u64> {
        let raw = text::search::parse(query);
        let mut n = 0;
        for entity_type in types {
            let text_index = text::resolve_built(self.r, entity_type, index, "search")?;
            let (within, live) = self.searched_places(entity_type)?;
            let matches =
                text::search::matches(self.r, entity_type, &text_index, &raw, options, within.as_ref(), live)?;
            n += matches.found.len();
        }
        Ok(n)
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
                    if !self.walk_type(entity_type, None, &mut keep)? {
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
            // and asked again for more while what it gave was not enough and it held more. Asked for as many items as
            // the graph has ever numbered, it has answered all it holds whatever it says, so a wrong `exhausted` ends
            // the asking rather than repeating it forever
            let numbered =
                super::vector::graph_of(self.r, entity_type, &def.name)?.map_or(0, |(_, next)| next as usize);
            // mutant-skip: replace \+ with \* in Query<'_, R>::nearest
            // nearest's first ask: it asks again, four times as many, while what it admitted is short of k and the
            // graph held more, so any first ask finds the same k
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
                if admitted.len() >= k || exhausted || ask >= numbered {
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
            let def = match text::resolve_built(self.r, entity_type, text_index, "hybrid") {
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
        // These scores replace a rank's before them, so the answer no longer says how that rank settled
        self.ranked.replace(None);
        {
            let mut hits = self.hits.borrow_mut();
            for (hit, name) in found {
                hits.insert(hit.id.clone(), (hit, name));
            }
        }
        each(ids, visit)
    }

    /// The entities of a start's types that the first `upto` of the filters the plan begins with keep, for stage
    /// `upto`, which reads for them all: each type's by turn, in creation order, as a walk of the type testing
    /// each filter would hand them on. The filters keep the same set in any order, so how they are read
    /// changes what is read and never what is answered.
    ///
    /// A filter the stages would refuse is refused before anything is read (`filters_taken`): none of their stages
    /// runs. With a page's `cursor`, only the entities past it are handed on, each type's lists sought there and not
    /// read up to it, and none when the cursor is no entity the filters keep
    fn filtered(&self, upto: usize, types: &[String], cursor: Option<&str>, visit: &mut Visit<'_>) -> Result<bool> {
        self.filters_taken(upto)?;
        let from = match cursor {
            Some(cursor) => {
                self.sought.set(Some(upto));
                match self.cursor_among(types, cursor)? {
                    Some(place) if self.passes(upto, cursor)? => Some(place),
                    _ => return Ok(true),
                }
            }
            None => None,
        };
        for (at, entity_type) in types.iter().enumerate() {
            let Some(after) = starts_past(from, at) else { continue };
            if !self.filtered_of(upto, entity_type, after, visit)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// One type's entities that the plan's first `upto` steps keep, filters all, those past the place `after`; false
    /// when `visit` stopped it.
    ///
    /// Each `where` of a value an index files, where the type has a built index on that one field, and each
    /// `withRole`, is a list of places in creation order (`listed`). An entity the filters keep is at a place every
    /// list holds, so the places the lists hold in common are read (`common`) and only the other filters are tested
    /// against the entity at each: what is read is bounded by the shortest list, and not the longest, in whichever
    /// order the filters are written (`common`). With no list the type's rows are walked and every filter tested. A plan of
    /// the trash reads the type's trash as a list among the others, where an index's entries are of every entity.
    ///
    /// A `range` of numbers an index reads (`ranges_of`) is no such list: its entries are in the numbers' order, so
    /// they are all read and sorted by place before one is handed on. They are read once reading them costs no
    /// more than this read has read already, which is asked each time it has read twice what it had, from
    /// `RANGE_ASKED` entries on, their number counted as a probe and no further than that; and before anything is
    /// read, one holding fewer than `RANGE_ASKED` is, but for a plan of the trash, most of whose entries would be
    /// of entities it doesn't reach. So a range holding a handful is read at once, a read that stops at its first
    /// match reads no range holding more than it had read by then, and the whole costs a small multiple of the
    /// cheaper way, whatever the store holds. Until it is read the range is tested against each entity, as any
    /// filter is. A read that takes every entity the filters keep, and has no list, reads a range whatever it
    /// holds, its rows being no fewer: the one holding the fewest, each counted to twice as far until one is
    /// known to
    fn filtered_of(&self, upto: usize, entity_type: &str, after: Option<u64>, visit: &mut Visit<'_>) -> Result<bool> {
        let mut read = 0u64;
        let ended = self.filtered_from(upto, entity_type, after, &mut read, visit);
        self.handed(read);
        ended
    }

    /// `filtered_of`, adding to `read` each place it is handed: one walked, one sought, one of a range read
    fn filtered_from(
        &self,
        upto: usize,
        entity_type: &str,
        after: Option<u64>,
        read: &mut u64,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        let mut lists = self.listed(upto, entity_type)?;
        let mut ranged = self.ranges_of(upto, entity_type)?;
        let trash = self.plan.scope == Scope::Trashed;
        if trash && !lists.is_empty() {
            lists.insert(0, Listed::new(Places::rows(keys::type_range(true, entity_type)), None, Vec::new()));
        }
        // The least place not yet handed on or ruled out
        let mut next = after.map_or(0, |place| place + 1);
        let mut ask_at = RANGE_ASKED;
        // A range's entries are of every entity, so a plan of the trash reads none it hasn't paid for
        self.read_ranges(&mut lists, &mut ranged, read, if trash { 0 } else { RANGE_ASKED })?;
        if !trash && !self.stops_early() {
            // Counted to twice as far each time, until the one holding the fewest is known, and so read: one
            // range alone is the fewest
            let mut upto = RANGE_ASKED;
            while lists.is_empty() && ranged.len() > 1 {
                upto *= 2;
                self.read_ranges(&mut lists, &mut ranged, read, upto)?;
            }
            if lists.is_empty() && !ranged.is_empty() {
                lists.push(self.read_range(ranged.remove(0))?);
            }
        }
        loop {
            let tested: Vec<usize> =
                (0..upto).filter(|at| !lists.iter().any(|list| list.covers.contains(at))).collect();
            if let Some(stages) = self.stages {
                if lists.is_empty() {
                    stages.reads_index("none: walks the type");
                }
                for index in lists.iter().filter_map(|list| list.index.as_deref()) {
                    stages.reads_index(index);
                }
                if lists.len() > 1 {
                    stages.read_in_common();
                }
            }
            let pause_at = if ranged.is_empty() { u64::MAX } else { ask_at };
            if lists.is_empty() {
                let (mut stopped, mut paused) = (false, None);
                self.walk_places(entity_type, next.checked_sub(1), &mut |place, id| {
                    *read += 1;
                    if self.kept(&tested, id)? && !visit(id)? {
                        stopped = true;
                        return Ok(false);
                    }
                    if *read >= pause_at {
                        paused = Some(place);
                        return Ok(false);
                    }
                    Ok(true)
                })?;
                if stopped {
                    return Ok(false);
                }
                let Some(place) = paused else { return Ok(true) };
                next = place + 1;
            } else {
                let asks = Asks { entity_type, tested: &tested, pause_at, kept: None };
                match self.common(&mut lists, &asks, &mut next, read, visit)? {
                    Left::Stopped => return Ok(false),
                    Left::Ended => return Ok(true),
                    Left::Paused => {}
                }
            }
            self.read_ranges(&mut lists, &mut ranged, read, 0)?;
            while ask_at <= *read {
                ask_at *= 2;
            }
        }
    }

    /// Whether an index reads any of the plan's leading filters in any of a start's types: a list of places
    /// (`listed`) or a range's entries (`ranges_of`)
    fn reads_together(&self, types: &[String]) -> Result<bool> {
        if let Some(together) = self.together.get() {
            return Ok(*together);
        }
        // Asking after a range's index reads its bounds: the filter refused is the one its stage would have
        self.filters_taken(self.filters)?;
        let mut together = false;
        for entity_type in types {
            if !self.listed(self.filters, entity_type)?.is_empty()
                || !self.ranges_of(self.filters, entity_type)?.is_empty()
            {
                together = true;
                break;
            }
        }
        Ok(*self.together.get_or_init(|| together))
    }

    /// Whether the plan may take fewer entities than its leading filters keep: one that answers the first, or
    /// whether there is one, and one with a `limit` among the steps after them. A read of any other takes them all
    fn stops_early(&self) -> bool {
        matches!(self.plan.terminal, Terminal::First | Terminal::FirstRow { .. } | Terminal::Exists)
            || self.plan.steps[self.filters..].iter().any(|step| matches!(step, Step::Limit { .. }))
    }

    /// The lists of a type's places that the plan's first `upto` steps are read from as the store holds them, each
    /// standing for its filters. A `where` of a value an index files reads the value's entries in a built index on
    /// its field alone, and a `withRole` its role's in the role index: one value's places are held in order, so
    /// they are in creation order, an entity once. Equalities on every field of an index on several read its
    /// entries for those values as one list (`equal_index`), which stands for them all in place of their own: its
    /// places are among each of theirs. A `lacks` of a field with an index of its own reads the entities the index
    /// files as without a value
    fn listed(&self, upto: usize, entity_type: &str) -> Result<Vec<Listed>> {
        let mut lists = Vec::new();
        let mut equals: Vec<(usize, &str, Part)> = Vec::new();
        for (at, step) in self.plan.steps[..upto].iter().enumerate() {
            let (name, part) = match step {
                Step::Where { field, value } => match indexable(field, value) {
                    Some(part) => {
                        equals.push((at, field.as_str(), part.clone()));
                        (field.as_str(), part)
                    }
                    None => continue,
                },
                Step::WithRole { role } => (field::ROLE, Part::String(role.clone())),
                Step::Lacks { field } => {
                    // The entities an index on the field files as without a value are those it is missing in and
                    // those it is null in, so the list narrows the read and the filter is still tested: it stands
                    // for no step
                    if let Some(index) = index_on(self.r, entity_type, field).filter(|_| accounted(field)) {
                        let own = keys::index_others(Prefix::IndexWithout, entity_type, &index, &[]);
                        lists.push(Listed::new(Places::filed(own), Some(index), Vec::new()));
                    }
                    continue;
                }
                _ => continue,
            };
            let Some(index) = index_on(self.r, entity_type, name) else { continue };
            let under = keys::index_range(entity_type, &index, std::slice::from_ref(&part));
            lists.push(Listed::new(Places::filed(under), Some(index), vec![at]));
        }
        if let Some(together) = self.equal_index(entity_type, &equals)? {
            lists.retain(|list| {
                list.covers.is_empty() || !list.covers.iter().all(|step| together.covers.contains(step))
            });
            lists.push(together);
        }
        Ok(lists)
    }

    /// The list of a type's places that equalities on every field of one of its indexes on several fields are
    /// read from, the index on the most fields: its entries under those values, which are keyed by place under
    /// the last field's as under any one value. An index is read so only while it holds, under the values of its
    /// fields before the last, no entity it doesn't order, which may hold the last one's in a list
    /// (`order_index`). `equals` are the equalities, each with its step; of two on one field the first is the
    /// index's
    fn equal_index(&self, entity_type: &str, equals: &[(usize, &str, Part)]) -> Result<Option<Listed>> {
        let Some(declared) = self.r.schema().types.get(entity_type) else { return Ok(None) };
        let asked = |name: &String| equals.iter().find(|(_, field, _)| field == name);
        let mut found: Option<Listed> = None;
        for definition in declared.indexes.values() {
            let crate::schema::IndexDef::Value { fields, .. } = definition else { continue };
            let Some((last, before)) = fields.split_last() else { continue };
            if before.is_empty() || found.as_ref().is_some_and(|list| list.covers.len() >= fields.len()) {
                continue;
            }
            let Some(all) = fields.iter().map(asked).collect::<Option<Vec<_>>>() else { continue };
            let leading: Vec<(&str, Part)> =
                all[..before.len()].iter().map(|(_, field, part)| (*field, part.clone())).collect();
            let Some((index, mut parts)) = self.order_index(entity_type, &leading, last)? else { continue };
            parts.push(all[before.len()].2.clone());
            let under = keys::index_range(entity_type, &index, &parts);
            found = Some(Listed::new(Places::filed(under), Some(index), all.iter().map(|(at, _, _)| *at).collect()));
        }
        Ok(found)
    }

    /// The entries an index holds for each `range` of numbers among the plan's first `upto` steps. The first of
    /// those, when they are equalities and a range over a start of this one type, are read together from an index
    /// on their fields (`range_index`): the most of them that one reads, its entries being among those of an index
    /// on any one of them. Each range after those is read from an index on its field alone that accounts for
    /// every entity of the type (`order_index`)
    fn ranges_of(&self, upto: usize, entity_type: &str) -> Result<Vec<Ranged>> {
        let mut found = Vec::new();
        let mut together = 0;
        for first in (1..=upto).rev() {
            if let Some((_, index, leading, limits)) = self.range_index(first)? {
                let (from, to) = limits.keys(entity_type, &index, &leading);
                found.push(Ranged { from, to, index, covers: (0..first).collect() });
                together = first;
                break;
            }
        }
        for at in together..upto {
            let Step::Range { field, .. } = &self.plan.steps[at] else { continue };
            let Bounds::Numbers(limits) = self.bounds_at(at)? else { continue };
            let Some((index, _)) = self.order_index(entity_type, &[], field)? else { continue };
            let (from, to) = limits.keys(entity_type, &index, &[]);
            found.push(Ranged { from, to, index, covers: vec![at] });
        }
        Ok(found)
    }

    /// Reads what of `ranged` this read has paid for, and makes each a list among `lists`: one holding fewer
    /// entries than the read has `read`, or than `least` where it has read fewer, counted as a probe and no
    /// further than that. The one holding the fewest is read, and its entries are then among what was read: so of
    /// two ranges that each hold a handful, the one holding fewer is read and the other tested against its
    /// entities. The lists of the steps a range stands for give way to it: its entries are among theirs
    fn read_ranges(&self, lists: &mut Vec<Listed>, ranged: &mut Vec<Ranged>, read: &mut u64, least: u64) -> Result<()> {
        let mut upto = (*read).max(least);
        loop {
            let mut fewest: Option<(usize, u64)> = None;
            for (at, range) in ranged.iter().enumerate() {
                let held = self.counted_within(&range.from, &range.to, upto, true)?;
                if held < upto && fewest.is_none_or(|(_, fewer)| held < fewer) {
                    fewest = Some((at, held));
                }
            }
            let Some((at, held)) = fewest else { return Ok(()) };
            let list = self.read_range(ranged.remove(at))?;
            lists
                .retain(|other| other.covers.is_empty() || !other.covers.iter().all(|step| list.covers.contains(step)));
            lists.push(list);
            *read += held;
            upto = *read;
        }
    }

    /// The places an index files under the values of a range, as a list of them in order: the places under each
    /// value are read, and no entity
    fn read_range(&self, ranged: Ranged) -> Result<Listed> {
        let mut places: Vec<u64> = Vec::new();
        super::places::visit(self.r, &ranged.from, &ranged.to, false, &mut |_, place| {
            places.push(place);
            Ok(true)
        })?;
        // An entity under several of the values, as one holding a list of them is, is at one place
        places.sort_unstable();
        places.dedup();
        Ok(Listed::new(Places::Held { places, passed: 0 }, Some(ranged.index), ranged.covers))
    }

    /// Hands on the entities at the places every one of `lists` holds, from `next` on, in order: those the scope
    /// reaches, that each of the filters `asks` names keeps.
    ///
    /// One list is walked, and each place it holds that isn't ruled out is sought in the others, one after
    /// another. A list that doesn't hold the place answers with the next it does hold, and no place before that
    /// one is in them all: the walk goes on to it, and the list after is the first asked there, so that each is
    /// asked in its turn and the shortest ends the read whichever it is. What the walked list holds before that
    /// place is passed over as it comes, and sought past once `PASSED_OVER` entries have been.
    ///
    /// The list walked is the one whose places have been furthest apart, one from the next, where the read has seen
    /// them (`Spread`): where each list's first place is says so to begin with, and a list whose places come to
    /// be twice as far apart as the walked one's is walked in its place. Walking the list with the fewest places
    /// reads the fewest entries, an entry walked being the next one where a place sought is found from the root.
    /// So for each place the shortest list holds, the read costs each other list an entry sought, and the walked
    /// one as many as `PASSED_OVER` passed over and one sought: a bound by the shortest list. Which list is walked
    /// follows from what the read has seen, which the order the filters are written in decides to begin with: so
    /// that order can change what is read, within the bound.
    ///
    /// A tracked read depends on every key it read: a list's own key, and each block of its places the read came
    /// to, with the keys between a block sought and the one that answered, to the end of the list's keys where
    /// it answered with none; and the row at each place the lists shared.
    ///
    /// A tracked read misses no write that changes what was handed on. Every place up to where the read stopped
    /// either was handed on, or was ruled out by a list that answered with a later place or with none: the keys
    /// that list read cover every place between the one sought and the one answered, so an entity can only come
    /// to be at a place every list holds by a write to a key that was read.
    ///
    /// `next` is kept at the least place neither handed on nor ruled out, and `read` grows by each entry read: the
    /// read pauses when it reaches what `asks` says to, to be taken up again from `next`
    fn common(
        &self,
        lists: &mut [Listed],
        asks: &Asks<'_>,
        next: &mut u64,
        read: &mut u64,
        visit: &mut Visit<'_>,
    ) -> Result<Left> {
        if lists.len() > 1 {
            // No place before the last of the lists' first is in them all
            let mut first = *next;
            for list in lists.iter_mut() {
                let Some(found) = list.ask(self.r, *next, read)? else { return Ok(Left::Ended) };
                first = first.max(found);
            }
            *next = first;
        }
        let mut turn = 0;
        loop {
            let mut furthest = 0;
            for at in 1..lists.len() {
                if lists[at].spread.wider(1, &lists[furthest].spread) {
                    furthest = at;
                }
            }
            lists.swap(0, furthest);
            let Some((walked, others)) = lists.split_first_mut() else { return Ok(Left::Ended) };
            let mut left = None;
            let mut passed = 0;
            let mut spread = walked.spread;
            let from = *next;
            let ran_out = walked.places.from(self.r, from, &mut |place| {
                *read += 1;
                spread.saw(from, place);
                if place < *next {
                    passed += 1;
                    return Ok(passed < PASSED_OVER);
                }
                passed = 0;
                for nth in 0..others.len() {
                    let at = (turn + nth) % others.len();
                    match others[at].ask(self.r, place, read)? {
                        None => {
                            left = Some(Left::Ended);
                            return Ok(false);
                        }
                        Some(found) if found > place => {
                            *next = found;
                            turn = at + 1;
                            if *read >= asks.pause_at {
                                left = Some(Left::Paused);
                            }
                            // One whose places are twice as far apart as this list's is walked in its place. A list
                            // sought is seen only where it was asked, so its places look at least as far apart as
                            // the walked one's leaps: with no margin the two would change places at every leap
                            return Ok(left.is_none() && !others[at].spread.wider(2, &spread));
                        }
                        Some(_) => {}
                    }
                }
                *next = place + 1;
                if let Some(kept) = asks.kept {
                    kept.borrow_mut().insert(place);
                    return Ok(true);
                }
                // The entity at the place, where the scope reaches one there: its type's rows say, and one they
                // name is an entity read
                let found = self.at_place(asks.entity_type, place)?;
                if found.is_some() {
                    self.count_visit();
                }
                if let Some(id) = found
                    && self.kept(asks.tested, &id)?
                    && !visit(&id)?
                {
                    left = Some(Left::Stopped);
                    return Ok(false);
                }
                if *read >= asks.pause_at {
                    left = Some(Left::Paused);
                }
                Ok(left.is_none())
            })?;
            walked.spread = spread;
            match left {
                Some(left) => return Ok(left),
                None if ran_out => return Ok(Left::Ended),
                // Too many entries were passed over, or another list is the one to walk: the walk begins again
                None => {}
            }
        }
    }

    /// Refuses what a stage of the first `upto` steps, filters, would refuse as it runs: a `withRole` naming no
    /// role the schema declares (`UNKNOWN_ROLE`), and a `range` whose bounds are none a range takes
    /// (`BATCH_INVALID`). The last written is refused first, as its stage is the first to run. A read that tests
    /// those filters against an index's matches runs none of their stages, and would refuse one only once a match
    /// reached it
    fn filters_taken(&self, upto: usize) -> Result<()> {
        for (at, step) in self.plan.steps[..upto].iter().enumerate().rev() {
            match step {
                Step::WithRole { role } if !self.r.schema().roles.contains_key(role) => {
                    return Err(Error::new(
                        ErrorCode::UnknownRole,
                        format!("withRole(\"{role}\"): not a role the schema declares"),
                    ));
                }
                Step::Range { .. } => {
                    self.bounds_at(at)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The bounds of the `range` at step `at`, read from the step once a read: a filter tested against each entity
    /// an index hands on (`passes`) reads no bounds again, nor makes a collator for strings each time. Refuses bounds
    /// that are none a range takes (`BATCH_INVALID`)
    fn bounds_at(&self, at: usize) -> Result<&Bounds> {
        if let Some(bounds) = self.bounds[at].get() {
            return Ok(bounds);
        }
        let Step::Range { at_least, above, at_most, below, .. } = &self.plan.steps[at] else {
            unreachable!("bounds are a range's")
        };
        let bounds = Bounds::of(&self.r.schema().collation, at_least, above, at_most, below)?;
        Ok(self.bounds[at].get_or_init(|| bounds))
    }

    /// Whether every one of the first `upto` steps, a type start's leading filters, keeps `id`
    fn passes(&self, upto: usize, id: &str) -> Result<bool> {
        for at in 0..upto {
            if !self.keeps(at, id)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether each of the filters at the steps `tested` keeps `id`
    fn kept(&self, tested: &[usize], id: &str) -> Result<bool> {
        for at in tested {
            if !self.keeps(*at, id)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether the filter at step `at`, one of those a type start begins with, keeps `id`
    fn keeps(&self, at: usize, id: &str) -> Result<bool> {
        Ok(match &self.plan.steps[at] {
            Step::OfType { entity_type } => {
                id.strip_prefix(entity_type.as_str()).is_some_and(|rest| rest.starts_with('-'))
            }
            Step::Within { .. } => self.kept_within(at).contains(id),
            Step::Where { field: name, value } => self.matches(id, name, value)?,
            Step::Has { field: name } => get(self.r, id, name)?.is_some(),
            Step::Lacks { field: name } => get(self.r, id, name)?.is_none(),
            Step::WithRole { role } => self.matches(id, field::ROLE, &Value::from(role.as_str()))?,
            Step::Range { field: name, .. } => self.within(id, name, self.bounds_at(at)?)?,
            _ => unreachable!("a type start's leading filters are filters alone"),
        })
    }

    /// Where a page's cursor is among the entities of a start's types: its type's place among them, the first when
    /// the type is named twice, and its own place in creation order. None for an entity of another type, and for one
    /// the scope doesn't reach
    fn cursor_among(&self, types: &[String], cursor: &str) -> Result<Option<(usize, u64)>> {
        let Some(at) = types.iter().position(|t| t == ids::type_of(cursor)) else { return Ok(None) };
        Ok(self.admitted(cursor)?.map(|seq| (at, seq)))
    }

    /// Hands on the entity of a type at a place an index names in an order, when the scope reaches one there;
    /// false when `visit` stopped the walk. The place counts as a visit either way: what a walk of an order costs
    /// is the places it reads, one of an entity the scope leaves out as one it hands on
    fn hand(&self, entity_type: &str, place: u64, visit: &mut Visit<'_>) -> Result<bool> {
        self.count_visit();
        match self.at_place(entity_type, place)? {
            Some(id) => visit(&id),
            None => Ok(true),
        }
    }

    /// The one type whose entities a plan reads in an order from an index: the start's, for a plan of the live or of
    /// all. A plan of the trash walks the trash, which is the smaller, where an index holds every entity
    fn ordered_type(&self) -> Option<&String> {
        if self.plan.scope == Scope::Trashed {
            return None;
        }
        match self.start_types() {
            Some([entity_type]) => Some(entity_type),
            _ => None,
        }
    }

    /// What the first `before` steps ask of an index that reads an order or a range of `name` under them: the value
    /// each `where` asks of its field, as an index files it, and the number bounds of a `range` of `name` itself.
    /// None when one of them is any other step, a `where` of null or of a value no index files, or a second `range`:
    /// those are filtered as they come. No index's fields are those of two equalities on one field, or of one on
    /// `name`, so those find none to read
    fn asked(&self, before: usize, name: &str) -> Result<Option<Asked<'_>>> {
        // A step that is no filter is none of these, and the filters are refused as their stages would refuse
        // them, the last written first: reading a range's bounds here would refuse the first
        if before > self.filters {
            return Ok(None);
        }
        self.filters_taken(before)?;
        let mut equals: Vec<(&str, Part)> = Vec::new();
        let mut limits = None;
        for (at, step) in self.plan.steps[..before].iter().enumerate() {
            match step {
                Step::Where { field, value } => {
                    let Some(part) = indexable(field, value) else { return Ok(None) };
                    equals.push((field, part));
                }
                Step::Range { field, .. } if field == name && limits.is_none() => match self.bounds_at(at)? {
                    Bounds::Numbers(found) => limits = Some(found.clone()),
                    Bounds::Text(..) => return Ok(None),
                },
                _ => return Ok(None),
            }
        }
        Ok(Some((equals, limits)))
    }

    /// The index that reads what the first `upto` steps keep, when they are equalities and one range of a field's
    /// numbers over a start of one type, whichever of them was written first, and the type has an index on the
    /// equalities' fields and then the range's (`order_index`): with the type, what its entries for them start
    /// with, and the range's bounds. None when the steps are any others, or no such index is built
    #[allow(clippy::type_complexity)]
    fn range_index(&self, upto: usize) -> Result<Option<(&String, String, Vec<Part>, Limits<f64>)>> {
        let Some([entity_type]) = self.start_types() else { return Ok(None) };
        let ranged = self.plan.steps[..upto].iter().find_map(|step| match step {
            Step::Range { field, .. } => Some(field),
            _ => None,
        });
        let Some(name) = ranged else { return Ok(None) };
        let Some((equals, Some(limits))) = self.asked(upto, name)? else { return Ok(None) };
        let Some((index, leading)) = self.order_index(entity_type, &equals, name)? else { return Ok(None) };
        Ok(Some((entity_type, index, leading, limits)))
    }

    /// The built index of a type that reads the entities `equals` names in `name`'s order, with what its entries for
    /// them start with: a declared index whose last field is `name` and whose fields before it are the equalities',
    /// whichever order they were asked in, the values in the index's (`entities::ValueIndex::accounts`). It is read
    /// only while it holds, under those values, no entity it doesn't order (`entities::Other::Unordered`): what an
    /// entity under other values holds leaves this read alone. With no equality it is an index on `name` alone,
    /// which accounts for every entity of the type
    fn order_index(
        &self,
        entity_type: &str,
        equals: &[(&str, Part)],
        name: &str,
    ) -> Result<Option<(String, Vec<Part>)>> {
        let Some(declared) = self.r.schema().types.get(entity_type) else { return Ok(None) };
        for (index, definition) in &declared.indexes {
            let crate::schema::IndexDef::Value { fields, .. } = definition else { continue };
            let Some((last, before)) = fields.split_last() else { continue };
            if last != name || before.len() != equals.len() {
                continue;
            }
            let leading: Option<Vec<Part>> = before
                .iter()
                .map(|field| equals.iter().find(|(asked, _)| asked == field).map(|(_, part)| part.clone()))
                .collect();
            let Some(leading) = leading else { continue };
            if !super::derive::is_built(self.r, entity_type, index)? {
                continue;
            }
            // The key the entities it doesn't order under those values are filed under, which is there while any is
            if !self.r.contains(&keys::index_others(Prefix::IndexUnordered, entity_type, index, &leading))? {
                return Ok(Some((index.clone(), leading)));
            }
        }
        Ok(None)
    }

    /// How the set an `orderBy` of `name` sorts, the one after the first `before` steps, is read in that order from an
    /// index instead, or none when it has to be sorted. It is read when the set is one type's entities, live or with
    /// the trash, kept by nothing but equalities and a `range` of the field's own numbers, and the type has an index
    /// that reads those entities in the field's order (`order_index`)
    fn ordered(&self, before: usize, name: &str) -> Result<Option<Ordered<'_>>> {
        let Some(entity_type) = self.ordered_type() else { return Ok(None) };
        let Some((equals, limits)) = self.asked(before, name)? else { return Ok(None) };
        self.ordered_under(entity_type, &equals, limits, name)
    }

    /// A type's entities that `equals` names, within `limits` when a range keeps them, as an index reads them in
    /// `name`'s order: its entries for them, which hold no string. The index's order is then the step's: false, true
    /// and the numbers, each value's entities in place order, then the entities without a value. A string's order is
    /// the schema's collation's, which the order of an entry's bytes isn't, so entities holding one are sorted
    fn ordered_under<'q>(
        &self,
        entity_type: &'q String,
        equals: &[(&str, Part)],
        limits: Option<Limits<f64>>,
        name: &str,
    ) -> Result<Option<Ordered<'q>>> {
        let Some((index, leading)) = self.order_index(entity_type, equals, name)? else { return Ok(None) };
        let (from, to) = match &limits {
            Some(limits) => limits.keys(entity_type, &index, &leading),
            None => {
                let strings = keys::index_strings(entity_type, &index, &leading);
                let mut held = false;
                self.r.visit_within(&strings, &keys::index_past(entity_type, &index, &leading), |_, _| {
                    held = true;
                    false
                })?;
                if held {
                    return Ok(None);
                }
                let mut least = leading.clone();
                least.push(Part::Bool(false));
                (keys::index_range(entity_type, &index, &least), strings)
            }
        };
        Ok(Some(Ordered { entity_type, index, leading, from, to, without: limits.is_none() }))
    }

    /// Where an ordered read begins for a page after `cursor`, the order being of the set the first `under` steps
    /// keep: past the cursor's entry, or past its place among the entities without a value. None when the cursor
    /// isn't in the set, whose page then holds nothing: another type's entity, one the scope doesn't reach, or one
    /// those steps don't keep
    fn past_cursor(&self, under: usize, ordered: &Ordered<'_>, name: &str, cursor: &str) -> Result<Option<Begin>> {
        let Some((_, seq)) = self.cursor_among(std::slice::from_ref(ordered.entity_type), cursor)? else {
            return Ok(None);
        };
        if !self.passes(under, cursor)? {
            return Ok(None);
        }
        let value = get_value(self.r, cursor, name)?.filter(|value| !value.is_nil());
        Ok(Some(match value.as_ref().and_then(values::index_part) {
            Some(part) => {
                let mut parts = ordered.leading.clone();
                parts.push(part);
                Begin::PastEntry(keys::index_range(ordered.entity_type, &ordered.index, &parts), seq)
            }
            None => Begin::PastWithout(seq),
        }))
    }

    /// A type's entities in a field's order, read from its index as `ordered` found they can be: `each` is handed
    /// the place of every entity the index files as its place is read, so one stopping the walk leaves the rest
    /// unread (`hand` keeps those the scope reaches). Ascending, the values from the least, then the entities
    /// without a value; descending, those first, then the values from the greatest, each value's entities in place
    /// order, as a sort that keeps ties does. A page begins past its cursor, sought and not walked to
    fn in_order(&self, ordered: &Ordered<'_>, descending: bool, begin: Begin, each: &mut Entry<'_>) -> Result<bool> {
        if let Some(stages) = self.stages {
            stages.reads_index(&ordered.index);
        }
        if !descending {
            let from = match begin {
                Begin::PastWithout(place) => return self.without_value(ordered, Some(place), each),
                Begin::PastEntry(own, place) => {
                    // The rest of the cursor's value, then the values above it
                    if !self.places_after(&own, Some(place), each)? {
                        return Ok(false);
                    }
                    past(own)
                }
                Begin::First => ordered.from.clone(),
            };
            if !super::places::visit(self.r, &from, &ordered.to, false, &mut |_, place| each(place))? {
                return Ok(false);
            }
            return self.without_value(ordered, None, each);
        }
        let before = if let Begin::PastEntry(own, place) = begin {
            // The rest of the cursor's value, then the values below it
            if !self.places_after(&own, Some(place), each)? {
                return Ok(false);
            }
            own
        } else {
            let after = match begin {
                Begin::PastWithout(place) => Some(place),
                _ => None,
            };
            if !self.without_value(ordered, after, each)? {
                return Ok(false);
            }
            ordered.to.clone()
        };
        // By value from the greatest, and one value's in place order, as an order that keeps ties has them
        super::places::visit(self.r, &ordered.from, &before, true, &mut |_, place| each(place))
    }

    /// The entity an ordered read's index files first among those holding a value, or (`greatest`) the first in place
    /// order of those under its greatest value: what `least` and `greatest` answer of the set, read by the terminal,
    /// the stage `stage` and the one running. An entry of an entity the scope doesn't reach is passed over, so the
    /// read is of one entry more for each of those before the answer
    fn extreme(&self, ordered: &Ordered<'_>, greatest: bool, stage: usize) -> Result<Option<String>> {
        if let Some(stages) = self.stages {
            stages.reads_index(&ordered.index);
        }
        let mut found = None;
        // By value from the least or from the greatest, and one value's entities in place order either way
        super::places::visit(self.r, &ordered.from, &ordered.to, greatest, &mut |_, place| {
            self.count_visit();
            found = self.at_place(ordered.entity_type, place)?;
            Ok(found.is_none())
        })?;
        if let (Some(stages), Some(_)) = (self.stages, &found) {
            stages.kept(stage);
        }
        Ok(found)
    }

    /// The places filed under the key `own`, in order, all of them or those past the place `after`, sought and
    /// not walked to. False when `each` stopped the walk
    fn places_after(&self, own: &[u8], after: Option<u64>, each: &mut Entry<'_>) -> Result<bool> {
        let mut sought = text::postings::Sought::new(super::places::term(own.to_vec()));
        let from = match after {
            Some(after) => match after.checked_add(1) {
                Some(from) => from,
                None => return Ok(true),
            },
            None => 0,
        };
        super::places::from(self.r, &mut sought, from, each)
    }

    /// The entities without a value in an ordered read's field, in place order, all of them or those past the place
    /// `after`: none for a read a `range` keeps, which holds none of them. False when `each` stopped the walk
    fn without_value(&self, ordered: &Ordered<'_>, after: Option<u64>, each: &mut Entry<'_>) -> Result<bool> {
        if !ordered.without {
            return Ok(true);
        }
        let without = keys::index_others(Prefix::IndexWithout, ordered.entity_type, &ordered.index, &ordered.leading);
        self.places_after(&without, after, each)
    }

    /// How many ids the first `upto` steps hand on
    fn counted(&self, upto: usize) -> Result<u64> {
        let mut n = 0;
        self.pull(upto, &mut |_| {
            n += 1;
            Ok(true)
        })?;
        Ok(n)
    }

    /// How many entities of the start's types a `range` straight after it keeps, in the plan's scope, counted in their
    /// indexes: the places of the entries between its number bounds, each an entity's own, less the trash's as a
    /// search leaves them out of its matches, so no entity is read. None when the bounds are strings, or a type has
    /// no index that accounts for every entity by the field, and the entities have to be walked
    fn ranged(&self, types: &[String], name: &str, bounds: &Bounds) -> Result<Option<u64>> {
        let Bounds::Numbers(limits) = bounds else { return Ok(None) };
        let mut indexes = Vec::with_capacity(types.len());
        for t in types {
            let Some((index, _)) = self.order_index(t, &[], name)? else { return Ok(None) };
            indexes.push((t, index));
        }
        let mut n = 0;
        for (t, index) in indexes {
            let (among, live) = self.searched_places(t)?;
            let (from, to) = limits.keys(t, &index, &[]);
            let mut places = RoaringTreemap::new();
            super::places::visit(self.r, &from, &to, false, &mut |_, place| {
                self.count_visit();
                places.insert(place);
                Ok(true)
            })?;
            if let Some(among) = among {
                places &= among;
            }
            if live {
                places = text::search::untrashed(self.r, t, places)?;
            }
            n += places.len();
        }
        Ok(Some(n))
    }

    /// Whether the entity's field holds a value within a `range`'s bounds, or a list holding one
    fn within(&self, id: &str, name: &str, bounds: &Bounds) -> Result<bool> {
        Ok(get_value(self.r, id, name)?.is_some_and(|value| bounds.hold(&value)))
    }

    /// The first `take` ids of the set after the first `upto` steps in `name`'s order, found by walking the order and
    /// testing each entity against those steps, where a sort reads every entity they keep; or none when the walk
    /// isn't tried or is given up, and the set is sorted. It is tried when the steps are filters a type start begins
    /// with, which keep the same set whichever side of the order they stand, an index reads the type in `name`'s
    /// order, and reading the filters reads `WALK_BEFORE_ASKING` entities or more. It is given up once reading the
    /// filters would cost less than the walk has: asked each time the walk has read twice what it had, from
    /// `WALK_BEFORE_ASKING` on, it stops when the filters would read fewer than `WALK_FOR_EACH_READ` times that. So
    /// the two together cost a small multiple of the cheaper, where what the filters would read is taken as what
    /// the shortest of their lists holds (`filters_read_fewer`): lists that hold few places in common are read
    /// for less than that, and the walk is then given up later than it need be. A page's walk begins past
    /// its `cursor`, sought in the order's index, and holds nothing when the filters don't keep the cursor
    fn walked_first(
        &self,
        upto: usize,
        name: &str,
        descending: bool,
        take: usize,
        cursor: Option<&str>,
    ) -> Result<Option<Vec<String>>> {
        let filters = &self.plan.steps[..upto];
        let swaps = |step: &Step| {
            matches!(
                step,
                Step::OfType { .. }
                    | Step::Within { .. }
                    | Step::Where { .. }
                    | Step::Has { .. }
                    | Step::Lacks { .. }
                    | Step::WithRole { .. }
            )
        };
        if filters.is_empty() || !filters.iter().all(swaps) {
            return Ok(None);
        }
        let Some(entity_type) = self.ordered_type() else { return Ok(None) };
        // A role the schema doesn't declare is refused as its filter refuses it, which the walk never runs
        self.filters_taken(upto)?;
        // A page's cursor that is no entity of the set has no page, whichever way the set would be read, and
        // nothing is read to say so: one of another type, one the scope doesn't reach, one the filters don't keep
        if let Some(cursor) = cursor
            && (self.cursor_among(std::slice::from_ref(entity_type), cursor)?.is_none()
                || !self.passes(upto, cursor)?)
        {
            return Ok(Some(Vec::new()));
        }
        let Some(ordered) = self.ordered_under(entity_type, &[], None, name)? else { return Ok(None) };
        if self.filters_read_fewer(entity_type, WALK_BEFORE_ASKING)? {
            return Ok(None);
        }
        let begin = match cursor {
            Some(cursor) => match self.past_cursor(upto, &ordered, name, cursor)? {
                Some(begin) => begin,
                None => return Ok(Some(Vec::new())),
            },
            None => Begin::First,
        };
        let mut found = Vec::new();
        let mut read = 0u64;
        let mut ask_at = WALK_BEFORE_ASKING;
        let mut given_up = false;
        self.in_order(&ordered, descending, begin, &mut |entry| {
            // Every entry read is counted, one of an entity the scope doesn't reach too: it cost the same
            read += 1;
            let wanted = self.hand(entity_type, entry, &mut |id| {
                if self.passes(upto, id)? {
                    found.push(id.to_owned());
                }
                Ok(found.len() < take)
            })?;
            // Asked once the entry is tested: a walk that has all it takes as it comes to ask is done, not given up
            if wanted && read == ask_at {
                if self.filters_read_fewer(entity_type, read * WALK_FOR_EACH_READ)? {
                    given_up = true;
                    return Ok(false);
                }
                ask_at *= 2;
            }
            Ok(wanted)
        })?;
        Ok((!given_up).then_some(found))
    }

    /// Whether reading a type start's leading filters reads fewer than `than` entities from the store, as nearly as a
    /// count says: whether one of the lists they are read from holds fewer (`listed`), the places they hold in
    /// common being no more than the shortest holds, though they may be far fewer; or with no list, whether the
    /// type holds fewer entities, the trash's among them for a plan of both. It is asked as a probe: it decides how
    /// the order is read and not what is answered, so a tracked read records nothing of it, and the keys it counts,
    /// which are no entity's, are not among a plan's visits
    fn filters_read_fewer(&self, entity_type: &str, than: u64) -> Result<bool> {
        let listed = self.listed(self.filters, entity_type)?;
        let mut any = false;
        let owns = listed.iter().filter_map(|list| match &list.places {
            Places::Filed { own, .. } => Some((own.clone(), past(own.clone()))),
            _ => None,
        });
        for (from, to) in owns {
            any = true;
            if self.counted_within(&from, &to, than, true)? < than {
                return Ok(true);
            }
        }
        if any {
            return Ok(false);
        }
        let live = match self.r.probe(&keys::count(entity_type))? {
            Some(bytes) => values::decode(&bytes)?.as_u64().unwrap_or(0),
            None => 0,
        };
        if self.plan.scope != Scope::WithTrashed {
            return Ok(live < than);
        }
        // A walk of both reads the trash's rows too
        let (from, to) = prefix_range(&keys::type_range(true, entity_type));
        let trashed = self.counted_within(&from, &to, than.saturating_sub(live), false)?;
        Ok(live.saturating_add(trashed) < than)
    }

    /// How many places the value keys that are `from` or after it and before `to` hold between them, or with
    /// `places` unset how many keys there are, each a row: counted no further than `upto`, and on from where this
    /// read's last count of them stopped, so a question asked again of more counts each key once over all its
    /// askings. But for a value in blocks whose places the count stopped among: its blocks are counted only as
    /// far as `upto` takes, so the answer may pass `upto` by what a block holds, and an asking for more counts
    /// that value again
    fn counted_within(&self, from: &[u8], to: &[u8], upto: u64, places: bool) -> Result<u64> {
        let mut counted = self.counted.borrow_mut();
        let (so_far, next) = counted.entry((from.to_vec(), to.to_vec())).or_insert_with(|| (0, from.to_vec()));
        if *so_far >= upto {
            return Ok(*so_far);
        }
        let (mut more, mut past, mut among, mut failed) = (0u64, next.clone(), 0u64, None);
        self.r.probe_visit_within(next, to, |own, stored| {
            if places {
                let wanted = upto - (*so_far + more);
                match super::places::count_probed(self.r, own, stored, wanted) {
                    Ok((held, true)) => more += held,
                    // The count stopped among this value's blocks: the next begins at this key again
                    Ok((held, false)) => {
                        among = held;
                        return false;
                    }
                    Err(error) => {
                        failed = Some(error);
                        return false;
                    }
                }
            } else {
                more += 1;
            }
            past.clear();
            past.extend_from_slice(own);
            past.push(0);
            *so_far + more < upto
        })?;
        if let Some(error) = failed {
            return Err(error);
        }
        *so_far += more;
        *next = past;
        Ok(*so_far + among)
    }

    /// Tells a read asked to explain itself which value index would have read the order of `name` that stage `stage`
    /// sorts, the set after the first `before` steps: the fields the equalities before it ask of, then `name`. Only
    /// where such an index is all that is missing: one type's entities kept by equalities and a range of the field,
    /// a field declared a number or a boolean, or a time the engine writes, whose order an index's entries have, and
    /// no index of the type on those fields
    fn note_wanted(&self, stage: usize, before: usize, name: &str) -> Result<()> {
        let Some(stages) = self.stages else { return Ok(()) };
        let Some(entity_type) = self.ordered_type() else { return Ok(()) };
        let Some((equals, _)) = self.asked(before, name)? else { return Ok(()) };
        let Some(declared) = self.r.schema().types.get(entity_type) else { return Ok(()) };
        let in_order = name == field::CREATED_AT
            || name == field::UPDATED_AT
            || declared.fields.get(name).is_some_and(|field| {
                use crate::schema::ValueType;
                matches!(field.value, ValueType::Number | ValueType::Integer | ValueType::Boolean)
            });
        let held = declared.indexes.values().any(|index| match index {
            crate::schema::IndexDef::Value { fields, .. } => fields.split_last().is_some_and(|(last, leading)| {
                last == name
                    && leading.len() == equals.len()
                    && leading.iter().all(|field| equals.iter().any(|(asked, _)| asked == field))
            }),
            _ => false,
        });
        let fields: Vec<&str> = equals.iter().map(|(field, _)| *field).chain([name]).collect();
        // An index names a field once
        let once = fields.iter().enumerate().all(|(at, field)| !fields[..at].contains(field));
        // And only a field the schema lets a value index name: a declared one of a type an index holds, or a time
        // the engine writes
        let named = equals.iter().all(|(field, _)| {
            *field == field::CREATED_AT
                || *field == field::UPDATED_AT
                || declared.fields.get(*field).is_some_and(|declared| declared.value.is_value_indexable())
        });
        if in_order && !held && once && named {
            stages.wants(stage, fields.join(", "));
        }
        Ok(())
    }

    /// The first `take` ids after the first `upto` steps in `name`'s order, as sorting them all and taking the first
    /// would answer, equal values in the order they came: only `take` are held at once. A page's are the first past
    /// its `cursor` in that order: the ids ordered after the cursor's value, and those of its value that came after
    /// it, which is where a sort that keeps ties has them; none when the cursor never comes
    fn first_ordered(
        &self,
        upto: usize,
        name: &str,
        descending: bool,
        take: usize,
        cursor: Option<&str>,
    ) -> Result<Vec<String>> {
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
        // What the cursor is ordered by, and whether it has come: until it has, an id of its value is before it
        let after = match cursor {
            Some(cursor) => Some((cursor, sort_key(get_value(self.r, cursor, name)?.as_ref()))),
            None => None,
        };
        let mut come = false;
        self.pull(upto, &mut |id| {
            let key = sort_key(get_value(self.r, id, name)?.as_ref());
            if let Some((cursor, at)) = &after {
                match order(&key, at) {
                    Ordering::Less => return Ok(true),
                    Ordering::Equal if !come => {
                        come = id == *cursor;
                        return Ok(true);
                    }
                    _ => {}
                }
            }
            if bound.as_ref().is_some_and(|last| order(last, &key) != Ordering::Greater) {
                return Ok(true);
            }
            kept.push((key, id.to_owned()));
            // mutant-skip: replace >= with < in Query<'_, R>::first_ordered
            // The first of an order through this, or the full sort and its first: the same ids in the same order
            // whenever the ids held are cut, and EXPLAIN's counts the same; when they are cut, and the bound a cut
            // leaves, are only what it costs
            if kept.len() >= take.saturating_mul(2).max(1) {
                cut(&mut kept);
                // mutant-skip: replace == with != in Query<'_, R>::first_ordered (as above)
                bound = (kept.len() == take).then(|| kept[take - 1].0.clone());
            }
            Ok(true)
        })?;
        if after.is_some() && !come {
            return Ok(Vec::new());
        }
        cut(&mut kept);
        Ok(kept.into_iter().map(|(_, id)| id).collect())
    }

    /// Every id after the first `upto` steps
    /// A rank index's rules, refusing an index the schema doesn't declare (`UNKNOWN_INDEX`)
    fn rank_def(&self, index: &str) -> Result<&crate::schema::RankDef> {
        self.r
            .schema()
            .ranks
            .get(index)
            .ok_or_else(|| Error::new(ErrorCode::UnknownIndex, format!("No rank index is named \"{index}\"")))
    }

    /// A built rank index's totals, and the step its scores are reported at, recording what a `hits` answer and EXPLAIN
    /// say of its scores
    fn rank_index(&self, stage: usize, index: &str, def: &crate::schema::RankDef) -> Result<(ranks::Totals, f64)> {
        let totals = ranks::totals(self.r, index)?;
        let (error, step) = ranks::reported(totals.error(def.damping), totals.nodes);
        self.ranked.replace(Some(Scored::Ranked { index: index.to_owned(), error }));
        if let Some(stages) = self.stages {
            stages.reads_index_at(stage, index);
            stages.rank_error(stage, error);
        }
        Ok((totals, step))
    }

    /// Whether `limit` after a `ranked` first step reads the index's top from its score order: the plan starts from
    /// the index's live entities, its types in its order, so the order's ties are the set's
    fn reads_rank_top(&self, index: &str) -> Result<bool> {
        Ok(self.plan.scope == Scope::Live
            && self.r.schema().ranks.get(index).is_some_and(|def| self.start_types() == Some(def.types.as_slice()))
            && ranks::is_built(self.r, index)?)
    }

    /// The first `take` of a rank index's nodes by score, each a hit when the terminal answers them: read from the top of
    /// its order, which holds a cell's nodes by place. It reads on past the `take`th only through a cell wider than the
    /// order promises, which few nodes can be in (`ranks::order_key`); a graph whose scores are alike has most of its nodes
    /// in one narrower cell, which reading through would cost the whole index. Those wide cells are sorted by the estimates,
    /// and everything read when hits show them, stably, so equal scores keep their place as `ranked_by` keeps the set's.
    /// Cells narrow with the score, so past the first narrow one ids alone need no estimates
    fn rank_top(&self, index: &str, take: usize) -> Result<Vec<String>> {
        let def = self.rank_def(index)?;
        let (totals, step) = self.rank_index(1, index, def)?;
        let precision = ranks::order_precision(def.tolerance);
        let promised = 2.0 * def.tolerance * totals.estimates;
        let scored = matches!(self.plan.terminal, Terminal::Hits { .. });
        let mut read: Vec<(String, f64)> = Vec::with_capacity(take);
        let mut narrow = false;
        let mut estimated = 0;
        let mut step_to = |id: &[u8]| -> Result<bool> {
            let id = String::from_utf8(id.to_vec())
                .map_err(|_| Error::new(ErrorCode::Corrupted, "A rank index's order names no entity"))?;
            if narrow && !scored {
                read.push((id, 0.0));
                return Ok(read.len() < take);
            }
            let x = ranks::node(self.r, index, &id)?
                .ok_or_else(|| {
                    Error::new(ErrorCode::Corrupted, format!("{id} is in a rank index's order and no node"))
                })?
                .x;
            let wide = ranks::cell_wider_than(x, precision, promised);
            narrow = !wide;
            if read.len() >= take && read.last().is_some_and(|last| !ranks::same_cell(last.1, x, precision)) {
                return Ok(false);
            }
            read.push((id, x));
            estimated += 1;
            Ok(read.len() < take || wide)
        };
        let mut failed = None;
        self.r.visit(&keys::rank_orders(index), |_, id| {
            self.count_visit();
            step_to(id).unwrap_or_else(|e| {
                failed = Some(e);
                false
            })
        })?;
        if let Some(e) = failed {
            return Err(e);
        }
        read[..estimated].sort_by(|a, b| (b.1 / totals.estimates).total_cmp(&(a.1 / totals.estimates)));
        read.truncate(take);
        Ok(read
            .into_iter()
            .map(|(id, x)| {
                if scored {
                    self.record_rank_hit(&id, ranks::rounded(x / totals.estimates, step));
                }
                id
            })
            .collect())
    }

    fn record_rank_hit(&self, id: &str, score: f64) {
        let hit = Hit { id: id.to_owned(), seq: 0, score, terms: Vec::new(), fields: Vec::new() };
        self.hits.borrow_mut().insert(id.to_owned(), (hit, "ranked".to_owned()));
    }

    /// The scores a rank index not built yet will hold, within its tolerance: `rank` over the graph it declares, as a
    /// query planner scans while an index builds. It costs what `rank` does, for every read until the build ends, so a
    /// read answers the same and only slower. The step its scores are reported at comes with them
    fn computed_scores(
        &self,
        stage: usize,
        index: &str,
        def: &crate::schema::RankDef,
    ) -> Result<(HashMap<String, f64, Fixed>, f64)> {
        let plan = Plan {
            scope: Scope::Live,
            start: Start::Types { types: def.types.clone() },
            steps: vec![Step::Rank {
                kinds: def.relations.clone(),
                direction: def.direction,
                seeds: None,
                weights: def.weights,
                damping: def.damping,
                iterations: COMPUTED_RANK_ROUNDS,
                // A round moving the scores by `d` leaves them within `d * damping / (1 - damping)` of where they settle
                tolerance: def.tolerance * (1.0 - def.damping) / def.damping,
            }],
            terminal: Terminal::Hits { fields: None, highlights: false },
        };
        let query = Query {
            r: self.r,
            plan: &plan,
            visited: Cell::new(0),
            places: Cell::new(0),
            stages: None,
            hits: RefCell::default(),
            backward_hop: Cell::new(None),
            walked_back: Cell::new(None),
            asked: Cell::new(None),
            sought: Cell::new(None),
            ranked: RefCell::default(),
            within: plan.steps.iter().map(|_| OnceCell::new()).collect(),
            bounds: plan.steps.iter().map(|_| OnceCell::new()).collect(),
            counted: RefCell::default(),
            filters: leading_filters(&plan),
            together: OnceCell::new(),
        };
        query.pull(1, &mut |_| Ok(true))?;
        self.visited.set(self.visited.get() + query.visited.get());
        let scores: HashMap<String, f64, Fixed> =
            query.hits.into_inner().into_iter().map(|(id, (hit, _))| (id, hit.score)).collect();
        let (error, step) = ranks::reported(def.tolerance, scores.len() as u64);
        self.ranked.replace(Some(Scored::Ranked { index: index.to_owned(), error }));
        if let Some(stages) = self.stages {
            stages.reads_index_at(stage, index);
            stages.rank_error(stage, error);
            stages.rank_building(stage);
        }
        Ok((scores, step))
    }

    /// `ranked`: the set by the index's scores, highest first, ties in the set's order; an entity that isn't one of the
    /// index's nodes is refused. An index not built yet answers from the scores it will hold, or refuses if asked to
    fn ranked_by(
        &self,
        before: usize,
        upto: usize,
        index: &str,
        while_building: WhileBuilding,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        let def = self.rank_def(index)?;
        let (scores, step) = if ranks::is_built(self.r, index)? {
            let (totals, step) = self.rank_index(upto, index, def)?;
            (RankScores::Held(totals), step)
        } else if while_building == WhileBuilding::Refuse {
            return Err(Error::new(
                ErrorCode::IndexBuilding,
                format!("The rank index {index} is still being built, and this read asked not to compute its scores"),
            ));
        } else {
            let (scores, step) = self.computed_scores(upto, index, def)?;
            (RankScores::Computed(scores), step)
        };
        let mut ids = self.collect(before)?;
        if !self.holds_each_once(before) {
            let mut seen: HashSet<String, Fixed> = HashSet::default();
            ids.retain(|id| seen.insert(id.clone()));
        }
        let mut scored = Vec::with_capacity(ids.len());
        for id in ids {
            let score = match &scores {
                RankScores::Held(totals) => ranks::node(self.r, index, &id)?.map(|node| node.x / totals.estimates),
                RankScores::Computed(scores) => scores.get(&id).copied(),
            };
            let Some(score) = score else {
                return Err(Error::new(
                    ErrorCode::BatchInvalid,
                    format!(
                        "{id} isn't a node of the rank index {index}, whose nodes are the live entities of {}",
                        def.types.join(", ")
                    ),
                ));
            };
            scored.push((id, score));
        }
        // Stable: equal scores keep the set's order
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let hits = matches!(self.plan.terminal, Terminal::Hits { .. });
        for (id, score) in scored {
            if hits {
                self.record_rank_hit(&id, ranks::rounded(score, step));
            }
            if !visit(&id)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

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
        if self.backward_hop.get() == Some(before + 1) {
            // From one entity over one kind, its edges from the last are its ids reversed; otherwise the `reverse`
            // collects them
            let sources = self.collect(before)?;
            if let ([from], [kind]) = (sources.as_slice(), kinds) {
                // A kind with no key joins two entities by one edge at most, so its far ends need no dedupe
                let one_per_pair = relations::definition(self.r, kind)?.key.is_empty();
                self.walked_back.set(Some(before + 1));
                return self.walk_edges(from, kind, types, outward, Some(one_per_pair), &mut reached, visit);
            }
            for from in &sources {
                for kind in kinds {
                    if !self.walk_edges(from, kind, types, outward, None, &mut reached, visit)? {
                        return Ok(false);
                    }
                }
            }
            return Ok(true);
        }
        self.pull(before, &mut |from| {
            for kind in kinds {
                if !self.walk_edges(from, kind, types, outward, None, &mut reached, visit)? {
                    return Ok(false);
                }
            }
            Ok(true)
        })
    }

    /// Visits the far ends of `from`'s edges of `kind` a step may reach, each once, in place order, or from the last
    /// when `backward` says whether the kind joins a pair by one edge at most; false when `visit` stopped the walk. From
    /// the last, an end joined by several edges of a keyed kind is visited at its first edge's place, where the walk in
    /// place order visits it
    #[allow(clippy::too_many_arguments)]
    fn walk_edges(
        &self,
        from: &str,
        kind: &str,
        types: Option<&[String]>,
        outward: bool,
        backward: Option<bool>,
        reached: &mut HashSet<String>,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        let mut go = true;
        relations::visit_edges(self.r, from, outward, Some(kind), backward.is_some(), |place, far| {
            if far == from || !of_types(&far, types) || reached.contains(&far) {
                return Ok(true);
            }
            if backward == Some(false) {
                let (source, target) = if outward { (from, far.as_str()) } else { (far.as_str(), from) };
                if relations::first_between(self.r, source, kind, target)? != Some(place) {
                    return Ok(true);
                }
            }
            reached.insert(far.clone());
            if self.alive(&far)? && !visit(&far)? {
                go = false;
            }
            Ok(go)
        })?;
        Ok(go)
    }

    fn reach(
        &self,
        before: usize,
        kinds: &[String],
        types: Option<&[String]>,
        outward: bool,
        depth: Option<u32>,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        for kind in kinds {
            relations::definition(self.r, kind)?;
        }
        let mut frontier = self.collect(before)?;
        let mut seen: HashSet<String> = frontier.iter().cloned().collect();
        let mut reached = Vec::new();
        let mut levels = 0;
        while !frontier.is_empty() && depth.is_none_or(|most| levels < most) {
            levels += 1;
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

    /// Visits an entity's neighbours: the far ends of its edges of the kinds on the sides given, each once in the order
    /// read, leaving out itself and any the scope doesn't reach, counting into `walked` each edge read. A far end is
    /// lent, and copied only to tell a repeat. Read through the plan's reader, so a tracked read is written into by a
    /// link or an unlink at the entity
    fn visit_neighbours(
        &self,
        id: &str,
        kinds: &[String],
        sides: &[bool],
        walked: &mut usize,
        mut each: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        let mut seen: HashSet<String, Fixed> = HashSet::default();
        for kind in kinds {
            for &outward in sides {
                relations::visit_far_ends(self.r, id, outward, Some(kind), |far, _| {
                    *walked += 1;
                    if far != id && !seen.contains(far) {
                        seen.insert(far.to_owned());
                        if self.alive(far)? {
                            each(far)?;
                        }
                    }
                    Ok(true)
                })?;
            }
        }
        Ok(())
    }

    /// The set after the first `before` steps as a graph for stage `stage`: its entities once each, in order, numbered
    /// so, and the edges of the kinds between two of them, followed the direction's ways, each entity's kind by kind and
    /// in place order. The edges are read through the plan's reader, so a tracked read is written into by a link or an
    /// unlink at any of the set's entities
    fn project(
        &self,
        before: usize,
        stage: usize,
        kinds: &[String],
        direction: Direction,
        weights: bool,
    ) -> Result<(Vec<String>, Projection)> {
        // Which of the kinds weigh their edges, when the read takes weights: the graph is weighted if any does, and an
        // edge of one that doesn't weighs 1
        let mut weighted = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let weighs = relations::definition(self.r, kind)?.weighted;
            weighted.push(weights && weighs);
        }
        let mut ids = self.collect(before)?;
        // A start of distinct types, nothing after it, walks each entity once; any other set may repeat one, kept at its
        // first place, its ids marked by reference rather than copied
        if !self.holds_each_once(before) {
            let first: Vec<bool> = {
                let mut seen: HashSet<&str, Fixed> = HashSet::default();
                ids.iter().map(|id| seen.insert(id.as_str())).collect()
            };
            let mut first = first.into_iter();
            ids.retain(|_| first.next().unwrap_or(false));
        }
        let place: HashMap<&str, u32, Fixed> =
            ids.iter().enumerate().map(|(at, id)| (id.as_str(), at as u32)).collect();
        let any_weighted = weighted.iter().any(|w| *w);
        let mut graph = if any_weighted { Projection::weighted() } else { Projection::new() };
        // One buffer for every node's targets and their weights, which `node` copies into the graph
        let mut targets: Vec<(u32, f64)> = Vec::new();
        for id in &ids {
            targets.clear();
            for (kind, &weighs) in kinds.iter().zip(&weighted) {
                for &outward in direction.sides() {
                    relations::visit_far_ends(self.r, id, outward, Some(kind), |far, weight| {
                        if let Some(&at) = place.get(far) {
                            targets.push((at, if weighs { weight.unwrap_or(1.0) } else { 1.0 }));
                        }
                        Ok(true)
                    })?;
                }
            }
            if any_weighted {
                graph.weighted_node(targets.drain(..));
            } else {
                graph.node(targets.drain(..).map(|(at, _)| at));
            }
        }
        if let Some(stages) = self.stages {
            stages.projected(stage, graph.nodes(), graph.edges(), graph.total_weight());
        }
        Ok((ids, graph))
    }

    /// A shortest path from an entity of the set to `to`, walked level by level as `reach` walks; nothing visited when
    /// there is none within `depth`
    fn path_to(
        &self,
        before: usize,
        to: &str,
        kinds: &[String],
        direction: Direction,
        depth: Option<u32>,
        visit: &mut Visit<'_>,
    ) -> Result<bool> {
        for kind in kinds {
            relations::definition(self.r, kind)?;
        }
        let mut frontier = self.collect(before)?;
        if frontier.iter().any(|id| id == to) {
            return visit(to);
        }
        // No path ends at an entity the scope doesn't reach, so nothing is walked looking for one
        if !self.alive(to)? {
            return Ok(true);
        }
        let mut seen: HashSet<String> = frontier.iter().cloned().collect();
        let mut parent: HashMap<String, String> = HashMap::new();
        let mut levels = 0;
        while !frontier.is_empty() && depth.is_none_or(|most| levels < most) {
            levels += 1;
            let mut next = Vec::new();
            for from in &frontier {
                for kind in kinds {
                    for &outward in direction.sides() {
                        for (_, far) in relations::edges(self.r, from, outward, Some(kind))? {
                            if seen.contains(&far) || !self.alive(&far)? {
                                continue;
                            }
                            seen.insert(far.clone());
                            parent.insert(far.clone(), from.clone());
                            if far == to {
                                let mut path = vec![far];
                                while let Some(previous) = parent.get(path.last().expect("a path holds `to`")) {
                                    path.push(previous.clone());
                                }
                                path.reverse();
                                return each(path, visit);
                            }
                            next.push(far);
                        }
                    }
                }
            }
            frontier = next;
        }
        Ok(true)
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
    /// Per stage that built a graph: its nodes and edges, and for a rank its rounds and whether it settled
    graph: RefCell<Vec<Option<Graphed>>>,
    /// Per `similar` stage: the neighbours of `to`, the entities it scored, and the edges it read
    compared: RefCell<Vec<Option<(usize, usize, usize)>>>,
    /// Per `ranked` stage: the bound on its scores' distance from PageRank
    errors: RefCell<Vec<Option<f64>>>,
    /// Per stage, whether a `ranked` answered from `rank` because its index isn't built yet
    building: RefCell<Vec<bool>>,
    /// Per `orderBy` stage that sorted for want of one: the fields of the value index that would read its order
    wants: RefCell<Vec<Option<String>>>,
    /// Per `search` stage: how many of its matches it held at once to rank them
    held: RefCell<Vec<Option<u64>>>,
    /// Per stage that read lists of places: how many places it was handed, each walked or sought; whether it read
    /// the places several lists hold in common; and for a search, how many places the set it ran within holds
    places: Vec<Cell<u64>>,
    common: RefCell<Vec<bool>>,
    within: RefCell<Vec<Option<u64>>>,
    running: Cell<usize>,
    since: Cell<std::time::Instant>,
    started: std::time::Instant,
}

#[derive(Clone, Copy)]
struct Graphed {
    nodes: usize,
    edges: usize,
    /// The weight of every edge added up, for a graph built with weights
    weight: Option<f64>,
    rounds: Option<(u32, bool)>,
    cycles: Option<u32>,
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
            graph: RefCell::new(vec![None; n]),
            compared: RefCell::new(vec![None; n]),
            errors: RefCell::new(vec![None; n]),
            building: RefCell::new(vec![false; n]),
            wants: RefCell::new(vec![None; n]),
            held: RefCell::new(vec![None; n]),
            places: (0..n).map(|_| Cell::new(0)).collect(),
            common: RefCell::new(vec![false; n]),
            within: RefCell::new(vec![None; n]),
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
        self.reads_index_at(self.running.get(), name);
    }

    /// The index stage `at` read, which a stage another reads for names (`limit` reading a `ranked` index's top)
    fn reads_index_at(&self, at: usize, name: &str) {
        let mut index = self.index.borrow_mut();
        index[at] = Some(match index[at].take() {
            Some(held) if held.split(", ").all(|one| one != name) => format!("{held}, {name}"),
            Some(held) => held,
            None => name.to_owned(),
        });
    }

    fn projected(&self, stage: usize, nodes: usize, edges: usize, weight: Option<f64>) {
        self.graph.borrow_mut()[stage] = Some(Graphed { nodes, edges, weight, rounds: None, cycles: None });
    }

    fn ranked(&self, stage: usize, iterations: u32, converged: bool) {
        if let Some(graphed) = &mut self.graph.borrow_mut()[stage] {
            graphed.rounds = Some((iterations, converged));
        }
    }

    fn rank_building(&self, stage: usize) {
        self.building.borrow_mut()[stage] = true;
    }

    fn wants(&self, stage: usize, fields: String) {
        self.wants.borrow_mut()[stage] = Some(fields);
    }

    /// How many matches the search running holds to rank
    fn held(&self, matches: usize) {
        self.held.borrow_mut()[self.running.get()] = Some(matches as u64);
    }

    /// The stage running was handed this many places of lists more
    fn handed(&self, places: u64) {
        let at = self.running.get();
        self.places[at].set(self.places[at].get() + places);
    }

    /// The stage running read the places several lists hold in common
    fn read_in_common(&self) {
        self.common.borrow_mut()[self.running.get()] = true;
    }

    /// The stage running, a search, ran within a set of this many places more
    fn ran_within(&self, places: u64) {
        let within = &mut self.within.borrow_mut()[self.running.get()];
        *within = Some(within.unwrap_or(0) + places);
    }

    fn rank_error(&self, stage: usize, error: f64) {
        self.errors.borrow_mut()[stage] = Some(error);
    }

    fn compared(&self, stage: usize, neighbours: usize, candidates: usize, edges: usize) {
        self.compared.borrow_mut()[stage] = Some((neighbours, candidates, edges));
    }

    fn cycled(&self, stage: usize, cycles: u32) {
        if let Some(graphed) = &mut self.graph.borrow_mut()[stage] {
            graphed.cycles = Some(cycles);
        }
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
        let graph = self.graph.borrow();
        let compared = self.compared.borrow();
        let errors = self.errors.borrow();
        let building = self.building.borrow();
        let wants = self.wants.borrow();
        let held = self.held.borrow();
        let common = self.common.borrow();
        let within = self.within.borrow();
        for at in 0..n {
            let name = match at {
                0 => start_name(&plan.start),
                _ if at == n - 1 => terminal_name(&plan.terminal),
                _ => step_name(&plan.steps[at - 1]),
            };
            // A stage visits what the stage before handed it, and what it read from the store itself
            let received = if at == 0 { 0 } else { self.kept[at - 1].get() };
            let graphed = graph[at];
            let extra = match graphed {
                None => 0,
                Some(Graphed { weight, rounds, cycles, .. }) => {
                    2 + u32::from(weight.is_some()) + 2 * u32::from(rounds.is_some()) + u32::from(cycles.is_some())
                }
            };
            let extra = extra
                + if compared[at].is_some() { 3 } else { 0 }
                + u32::from(errors[at].is_some())
                + u32::from(building[at])
                + u32::from(wants[at].is_some())
                + u32::from(held[at].is_some())
                + u32::from(self.places[at].get() > 0)
                + u32::from(common[at])
                + u32::from(within[at].is_some());
            w.map(if index[at].is_some() { 5 } else { 4 } + extra);
            w.str("stage");
            w.str(name);
            if let Some(index) = &index[at] {
                w.str("index");
                w.str(index);
            }
            if let Some(Graphed { nodes, edges, weight, rounds, cycles }) = graphed {
                w.str("nodes");
                w.uint(nodes as u64);
                w.str("edges");
                w.uint(edges as u64);
                if let Some(weight) = weight {
                    w.str("weight");
                    rmp::encode::write_f64(w.0, weight).expect("a Vec");
                }
                if let Some((iterations, converged)) = rounds {
                    w.str("iterations");
                    w.uint(u64::from(iterations));
                    w.str("converged");
                    w.bool(converged);
                }
                if let Some(cycles) = cycles {
                    w.str("cycles");
                    w.uint(u64::from(cycles));
                }
            }
            if let Some(error) = errors[at] {
                w.str("error");
                rmp::encode::write_f64(w.0, error).expect("a Vec");
            }
            if building[at] {
                w.str("building");
                w.bool(true);
            }
            if let Some(fields) = &wants[at] {
                w.str("wants");
                w.str(fields);
            }
            if let Some(held) = held[at] {
                w.str("held");
                w.uint(held);
            }
            if common[at] {
                w.str("set");
                w.str("and");
            }
            if self.places[at].get() > 0 {
                w.str("places");
                w.uint(self.places[at].get());
            }
            if let Some(within) = within[at] {
                w.str("within");
                w.uint(within);
            }
            if let Some((neighbours, candidates, edges)) = compared[at] {
                w.str("neighbours");
                w.uint(neighbours as u64);
                w.str("candidates");
                w.uint(candidates as u64);
                w.str("edges");
                w.uint(edges as u64);
            }
            w.str("visited");
            w.uint(received + self.read[at].get());
            w.str("kept");
            // The terminal keeps what it answers: what the last step handed it, and what it read from the store
            // itself, the entity `least` or `greatest` found in an index
            w.uint(if at == n - 1 { received + self.kept[at].get() } else { self.kept[at].get() });
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
        Step::Range { .. } => "range",
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
        Step::Both { .. } => "both",
        Step::Descendants { .. } => "descendants",
        Step::Ancestors { .. } => "ancestors",
        Step::PathTo { .. } => "pathTo",
        Step::Rank { .. } => "rank",
        Step::Ranked { .. } => "ranked",
        Step::Topological { .. } => "topological",
        Step::Similar { .. } => "similar",
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
        Terminal::Degrees { .. } => "degrees",
        Terminal::WeakComponents { .. } => "weakComponents",
        Terminal::StrongComponents { .. } => "strongComponents",
        Terminal::Communities { .. } => "communities",
        Terminal::GroupBy { .. } => "groupBy",
        Terminal::Least { .. } => "least",
        Terminal::Greatest { .. } => "greatest",
        Terminal::Sum { .. } => "sum",
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

/// How many filters a plan starts with: the steps that keep the same set in any order, which a type start reads
/// together (`Query::filtered`)
fn leading_filters(plan: &Plan) -> usize {
    let filters = |step: &&Step| {
        matches!(
            step,
            Step::OfType { .. }
                | Step::Within { .. }
                | Step::Where { .. }
                | Step::Has { .. }
                | Step::Lacks { .. }
                | Step::WithRole { .. }
                | Step::Range { .. }
        )
    };
    plan.steps.iter().take_while(filters).count()
}

fn each(ids: Vec<String>, visit: &mut Visit<'_>) -> Result<bool> {
    for id in ids {
        if !visit(&id)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Walks entries of the store through `each`, which answers whether to go on; `run` is the walk, handed what to give
/// each entry to. False when `each` stopped the walk, and its error when it failed
fn walk(
    run: impl FnOnce(&mut dyn FnMut(&[u8], &[u8]) -> bool) -> Result<()>,
    mut each: impl FnMut(&[u8], &[u8]) -> Result<bool>,
) -> Result<bool> {
    let mut outcome = Ok(true);
    run(&mut |key, value| match each(key, value) {
        Ok(true) => true,
        other => {
            outcome = other;
            false
        }
    })?;
    outcome
}

/// The least key above `key`: where a walk past an entry starts
fn past(mut key: Vec<u8>) -> Vec<u8> {
    key.push(0);
    key
}

/// Where the walk of the type at `at` among a start's types begins, for a page whose cursor is at `from`, its type's
/// place among them and its own: `None` for a type before the cursor's, which the page is past; past the cursor's
/// place in its own type; and from the first entity in a type after it, as in every type when there is no cursor
fn starts_past(from: Option<(usize, u64)>, at: usize) -> Option<Option<u64>> {
    match from {
        Some((first, _)) if at < first => None,
        Some((first, place)) if at == first => Some(Some(place)),
        _ => Some(None),
    }
}

/// Where a read of a type in a field's order begins: at its first entity, or for a page, past its cursor's entry in
/// the index or its place among the entities without a value
enum Begin {
    First,
    /// Past the place among those filed under the key
    PastEntry(Vec<u8>, u64),
    PastWithout(u64),
}

/// What the steps before an order or a range ask of the index that reads it: each equality's field and the value it
/// asks for, and the bounds of a range of the ordered field (`Query::asked`)
type Asked<'q> = (Vec<(&'q str, Part)>, Option<Limits<f64>>);

/// How many entities reading the filters must read for a walk of the order, testing each entity against them, to be
/// tried in its place, and how many the walk reads before it first asks whether reading the filters would cost less
/// (`Query::walked_first`): filters that read fewer are read, the walk having little to save
const WALK_BEFORE_ASKING: u64 = 128;

/// How many entities the filters' own reading may read for each one the walk has, and still be the cheaper: a walked
/// entity costs its record and each filter's field, about twice what a filter's match costs the sort, its field
/// alone (`docs/evidence/million-notes.md`, A filter before an order)
const WALK_FOR_EACH_READ: u64 = 2;

/// How many entries a read of a type start's leading filters reads before it first asks whether a range's index
/// holds fewer (`Query::filtered_of`), and how far it counts them before it has read anything: a range holding fewer
/// is read at once, for the price of counting that many keys
const RANGE_ASKED: u64 = 16;

/// How many entries of the list it walks a read of the places some lists hold in common passes over, one after
/// another, before it seeks the place the others are at (`Query::common`): an entry passed over is the next one
/// read, where a seek finds its way down from the root
const PASSED_OVER: u32 = 8;

/// A type's entities as a plan reads them in a field's order from a value index (`Query::ordered`)
struct Ordered<'q> {
    entity_type: &'q String,
    index: String,
    /// What the index's entries for the entities read start with: the values the equalities before the order ask
    /// for, as the index's fields have them, and nothing for an index on the order's field alone
    leading: Vec<Part>,
    /// The entries read: those from this key up to that one, the booleans and numbers under the leading parts, or
    /// the numbers a `range` keeps
    from: Vec<u8>,
    to: Vec<u8>,
    /// Whether the entities without a value are in the set, which they aren't after a `range`
    without: bool,
}

/// A lower and an upper bound, each with whether a value equal to it is within; none is no bound on that side
#[derive(Clone)]
struct Limits<T> {
    low: Option<(T, bool)>,
    high: Option<(T, bool)>,
}

impl<T> Limits<T> {
    /// The bounds two sides give, each a value and whether it is within, as `read` reads each value; none when it
    /// reads one of them as nothing
    fn of(
        low: Option<(&Value, bool)>,
        high: Option<(&Value, bool)>,
        read: impl Fn(&Value) -> Option<T>,
    ) -> Option<Self> {
        let side = |side: Option<(&Value, bool)>| match side {
            None => Some(None),
            Some((bound, within)) => read(bound).map(|bound| Some((bound, within))),
        };
        Some(Limits { low: side(low)?, high: side(high)? })
    }

    /// Whether a value is within the bounds, `order` saying how it compares with one of them
    fn hold(&self, order: impl Fn(&T) -> Ordering) -> bool {
        let above = self.low.as_ref().is_none_or(|(bound, within)| match order(bound) {
            Ordering::Greater => true,
            Ordering::Equal => *within,
            Ordering::Less => false,
        });
        let below = self.high.as_ref().is_none_or(|(bound, within)| match order(bound) {
            Ordering::Less => true,
            Ordering::Equal => *within,
            Ordering::Greater => false,
        });
        above && below
    }
}

impl Limits<f64> {
    /// The keys an index's entries for the numbers within the bounds lie between, under what the entries start with:
    /// from the first up to the second. A side without a bound ends where the numbers under those parts do
    fn keys(&self, entity_type: &str, index: &str, leading: &[Part]) -> (Vec<u8>, Vec<u8>) {
        let at = |bound: f64| {
            let mut parts = leading.to_vec();
            parts.push(Part::Number(bound));
            parts
        };
        let from = match self.low {
            Some((bound, true)) => keys::index_range(entity_type, index, &at(bound)),
            Some((bound, false)) => keys::index_past(entity_type, index, &at(bound)),
            None => keys::index_range(entity_type, index, &at(f64::NEG_INFINITY)),
        };
        let to = match self.high {
            Some((bound, true)) => keys::index_past(entity_type, index, &at(bound)),
            Some((bound, false)) => keys::index_range(entity_type, index, &at(bound)),
            None => keys::index_strings(entity_type, index, leading),
        };
        (from, to)
    }
}

/// What a `range` step keeps a field's values within: number bounds, which hold numbers and timestamps, a timestamp
/// by its milliseconds; or string bounds, which hold strings in the order of the collation with them
enum Bounds {
    Numbers(Limits<f64>),
    Text(Limits<String>, icu_collator::CollatorBorrowed<'static>),
}

impl Bounds {
    /// A `range` step's bounds, strings compared by `collation`. Refuses a step with no bound, with two on one side,
    /// with a bound that is no number, timestamp or string, or with a number on one side and a string on the other
    /// (`BATCH_INVALID`)
    fn of(
        collation: &str,
        at_least: &Option<Value>,
        above: &Option<Value>,
        at_most: &Option<Value>,
        below: &Option<Value>,
    ) -> Result<Self> {
        let refused = |what: &str| Error::new(ErrorCode::BatchInvalid, format!("range takes {what}"));
        let low = Self::side(at_least, above).ok_or_else(|| refused("atLeast or above, and was given both"))?;
        let high = Self::side(at_most, below).ok_or_else(|| refused("atMost or below, and was given both"))?;
        if low.is_none() && high.is_none() {
            return Err(refused("a bound: atLeast or above, atMost or below, or one of each"));
        }
        if let Some(limits) = Limits::of(low, high, |bound| values::as_time(bound).filter(|n| !n.is_nan())) {
            return Ok(Bounds::Numbers(limits));
        }
        if let Some(limits) = Limits::of(low, high, |bound| bound.as_str().map(str::to_owned)) {
            return Ok(Bounds::Text(limits, collator(collation)?));
        }
        Err(refused("bounds that are all numbers or all strings, a timestamp being a number of milliseconds"))
    }

    /// One side's bound and whether a value equal to it is within, from the bound that says so and the one that says
    /// not: no bound when neither is given, and none of this when both are
    fn side<'v>(within: &'v Option<Value>, outside: &'v Option<Value>) -> Option<Option<(&'v Value, bool)>> {
        match (within, outside) {
            (Some(_), Some(_)) => None,
            (Some(bound), None) => Some(Some((bound, true))),
            (None, Some(bound)) => Some(Some((bound, false))),
            (None, None) => Some(None),
        }
    }

    /// Whether a value is within the bounds, or is a list holding one that is
    fn hold(&self, value: &Value) -> bool {
        let one = |value: &Value| match self {
            Bounds::Numbers(limits) => values::as_time(value)
                .is_some_and(|n| limits.hold(|bound| n.partial_cmp(bound).unwrap_or(Ordering::Equal))),
            Bounds::Text(limits, collator) => {
                value.as_str().is_some_and(|text| limits.hold(|bound| collator.compare(text, bound)))
            }
        };
        match value {
            Value::Array(items) => items.iter().any(one),
            _ => one(value),
        }
    }
}

/// A graph answer's groups, under `kind`: each group's ids, its nodes named by `ids`, or with `counts` its size, read
/// without its members
fn write_groups(kind: &str, ids: &[String], groups: &Groups, counts: bool, w: &mut W<'_>) {
    w.map(2);
    w.kind(kind);
    w.str(kind);
    w.array(groups.len() as u32);
    if counts {
        for size in groups.sizes() {
            w.uint(u64::from(size));
        }
        return;
    }
    for group in groups.iter() {
        w.array(group.len() as u32);
        for &at in group {
            w.str(&ids[at as usize]);
        }
    }
}

fn of_types(id: &str, types: Option<&[String]>) -> bool {
    types.is_none_or(|ts| {
        ts.is_empty() || ts.iter().any(|t| id.strip_prefix(t.as_str()).is_some_and(|rest| rest.starts_with('-')))
    })
}

/// Whether an index on this field alone accounts for every entity of its type, filing those without a value: any but
/// the short code's and the roles', which hold only what their lookups read (`entities::ValueIndex::accounts`)
fn accounted(name: &str) -> bool {
    name != field::ROLE && name != field::SHORT_CODE
}

/// The built value index of the type a lookup of `name` reads: the built-in one for roles and short codes, or a
/// declared one on that field alone. An index not yet built isn't read: the lookup walks the type's rows
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

/// What kind of value `v` is, in words a message can name it by
fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Nil => "null",
        Value::Boolean(_) => "boolean",
        Value::Integer(_) | Value::F32(_) | Value::F64(_) => "number",
        Value::String(_) => "string",
        Value::Binary(_) => "binary",
        Value::Array(_) => "list",
        Value::Map(_) => "map",
        Value::Ext(t, _) if *t == TIMESTAMP_EXTENSION => "timestamp",
        Value::Ext(..) => "extension value",
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
    let q = Query {
        r,
        plan,
        visited: Cell::new(0),
        places: Cell::new(0),
        stages,
        hits: RefCell::new(HashMap::default()),
        backward_hop: Cell::new(None),
        walked_back: Cell::new(None),
        asked: Cell::new(None),
        sought: Cell::new(None),
        ranked: RefCell::new(None),
        within: plan.steps.iter().map(|_| OnceCell::new()).collect(),
        bounds: plan.steps.iter().map(|_| OnceCell::new()).collect(),
        counted: RefCell::default(),
        filters: leading_filters(plan),
        together: OnceCell::new(),
    };
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
            let n = match (&plan.start, plan.scope, plan.steps.as_slice()) {
                (Start::Types { types }, Scope::Live, []) => {
                    let mut n = 0;
                    for t in types {
                        n += entities::count(r, t)?;
                    }
                    n
                }
                // A search's matches are counted as the index holds them, where the walk would score and read each
                (Start::Types { types }, _, [Step::Search { query, index, options }]) => {
                    q.matched(types, query, index.as_deref(), options)?
                }
                // And a range's entries, where the walk would read each one's entity
                (Start::Types { types }, _, [Step::Range { field, at_least, above, at_most, below }]) => {
                    let bounds = Bounds::of(&r.schema().collation, at_least, above, at_most, below)?;
                    match q.ranged(types, field, &bounds)? {
                        Some(n) => n,
                        None => q.counted(last)?,
                    }
                }
                _ => q.counted(last)?,
            };
            w.map(2);
            w.kind("count");
            w.str("n");
            w.uint(n);
        }
        Terminal::First | Terminal::Exists => {
            // A search need keep only its best match for the first, and score none to say there is one
            q.asks(last, if plan.terminal == Terminal::Exists { Ask::Any } else { Ask::Best(1) });
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
            q.asks(last, Ask::Best(1));
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
        Terminal::Edges { kinds, direction, types } => {
            for kind in kinds {
                relations::definition(r, kind)?;
            }
            let ids = q.collect(last)?;
            let mut rows = Vec::new();
            // A relation is met twice only when the read follows both sides of the set's edges, meeting one between two
            // of its entities from each, or when the set holds an entity twice: only then are those answered kept, and
            // a relation met again is passed over before its far end is read
            let repeats = *direction == Direction::Both || !q.holds_each_once(last);
            let mut answered: HashSet<String, Fixed> = HashSet::default();
            for from in &ids {
                for kind in kinds {
                    for &outward in direction.sides() {
                        for (relation, far) in relations::edges(r, from, outward, Some(kind))? {
                            if repeats && answered.contains(&relation) {
                                continue;
                            }
                            if of_types(&far, types.as_deref()) && q.alive(&far)? {
                                let stored = relations::get(r, &relation)?.expect("an edge's relation exists");
                                if repeats {
                                    answered.insert(relation.clone());
                                }
                                rows.push((relation, stored));
                            }
                        }
                    }
                }
            }
            w.map(2);
            w.kind("relations");
            w.str("relations");
            w.array(rows.len() as u32);
            for (id, stored) in &rows {
                write_relation(r, id, stored, &mut w);
            }
        }
        Terminal::Degrees { kinds, direction, types } => {
            for kind in kinds {
                relations::definition(r, kind)?;
            }
            let ids = q.collect(last)?;
            w.map(2);
            w.kind("degrees");
            w.str("degrees");
            w.array(ids.len() as u32);
            for from in &ids {
                let mut n = 0u64;
                for kind in kinds {
                    for &outward in direction.sides() {
                        for (_, far) in relations::edges(r, from, outward, Some(kind))? {
                            if of_types(&far, types.as_deref()) && q.alive(&far)? {
                                n += 1;
                            }
                        }
                    }
                }
                w.array(2);
                w.str(from);
                w.uint(n);
            }
        }
        Terminal::WeakComponents { kinds, counts } => {
            let (ids, graph) = q.project(last, last + 1, kinds, Direction::Out, false)?;
            write_groups("weakComponents", &ids, &algorithms::weak_components(&graph), *counts, &mut w);
        }
        Terminal::StrongComponents { kinds, counts } => {
            let (ids, graph) = q.project(last, last + 1, kinds, Direction::Out, false)?;
            write_groups("strongComponents", &ids, &algorithms::strong_components(&graph), *counts, &mut w);
        }
        Terminal::Communities { kinds, resolution, seed, iterations, weights } => {
            // NaN is not finite
            if !(resolution.is_finite() && *resolution >= 0.0) || *iterations == 0 {
                return Err(Error::new(
                    ErrorCode::BatchInvalid,
                    format!(
                        "communities takes a resolution of at least 0 and at least one iteration, and was given \
                         {resolution} and {iterations}"
                    ),
                ));
            }
            let (ids, graph) = q.project(last, last + 1, kinds, Direction::Out, *weights)?;
            let groups = algorithms::communities(&graph, *resolution, u64::from(*seed), *iterations);
            write_groups("communities", &ids, &groups, false, &mut w);
        }
        Terminal::Least { field: name } | Terminal::Greatest { field: name } => {
            let greatest = matches!(plan.terminal, Terminal::Greatest { .. });
            // A set an index reads in the field's order, as an `orderBy` of it would be read, is answered by the
            // first entry under the index's least value or under its greatest, and nothing else of the set is read.
            // Any other set is read whole
            let best = match q.ordered(last, name)? {
                Some(ordered) => match q.extreme(&ordered, greatest, last + 1)? {
                    Some(id) => get_value(r, &id, name)?.map(|value| (id, value)),
                    None => None,
                },
                None => {
                    let ids = q.collect(last)?;
                    let collator = collator(&r.schema().collation)?;
                    // The first of equal values in the plan's order stays: a value replaces the best only when it is
                    // past it
                    let mut best: Option<(SortKey, String, Value)> = None;
                    for id in ids {
                        let Some(value) = get_value(r, &id, name)?.filter(|value| !value.is_nil()) else { continue };
                        let key = sort_key(Some(&value));
                        let past = match &best {
                            None => true,
                            Some((held, _, _)) => {
                                let order = compare(&collator, &key, held);
                                if greatest { order == Ordering::Greater } else { order == Ordering::Less }
                            }
                        };
                        if past {
                            best = Some((key, id, value));
                        }
                    }
                    best.map(|(_, id, value)| (id, value))
                }
            };
            w.map(3);
            w.kind(if greatest { "greatest" } else { "least" });
            w.str("id");
            match &best {
                Some((id, _)) => w.str(id),
                None => w.nil(),
            }
            w.str("value");
            match &best {
                Some((_, value)) => w.raw(&values::encode(value)),
                None => w.nil(),
            }
        }
        Terminal::Sum { field: name } => {
            let ids = q.collect(last)?;
            // Kept exactly and rounded once (`sum.rs`): the nearest number to the sum, whatever the set's order
            let (mut sum, mut n, mut within) = (super::sum::Sum::default(), 0u64, true);
            for id in ids {
                let Some(value) = get_value(r, &id, name)?.filter(|value| !value.is_nil()) else { continue };
                let Some(number) = values::as_number(&value) else {
                    return Err(Error::new(
                        ErrorCode::ValueInvalid,
                        format!(
                            "sum({name}) needs numbers, and {id} holds a {}: change its field or sum another field",
                            value_kind(&value)
                        ),
                    ));
                };
                // Past what a number holds there is nothing to add to: the rest are read for a value that is no
                // number, which is refused first, and the sum is refused after them
                within = within && sum.add(number);
                n += 1;
            }
            if !within {
                return Err(Error::new(
                    ErrorCode::ValueInvalid,
                    format!("sum({name}) passes what a number holds: sum fewer or smaller values of {name}"),
                ));
            }
            let sum = sum.total();
            w.map(3);
            w.kind("sum");
            w.str("sum");
            rmp::encode::write_f64(w.0, sum).expect("a Vec");
            w.str("n");
            w.uint(n);
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
                matches!(
                    step,
                    Step::Search { .. }
                        | Step::Near { .. }
                        | Step::Like { .. }
                        | Step::Hybrid { .. }
                        | Step::Rank { .. }
                        | Step::Ranked { .. }
                        | Step::Similar { .. }
                )
            };
            if !plan.steps.iter().any(searches) {
                return Err(Error::new(
                    ErrorCode::BatchInvalid,
                    "hits() answers a search, a rank or a similar, and the plan has none",
                ));
            }
            let ids = q.collect(last)?;
            let hits = q.hits.borrow();
            let ranked = q.ranked.borrow();
            w.map(if ranked.is_some() { 4 } else { 3 });
            w.kind("hits");
            match &*ranked {
                Some(Scored::Rank { converged, iterations }) => {
                    w.str("rank");
                    w.map(2);
                    w.str("converged");
                    w.bool(*converged);
                    w.str("iterations");
                    w.uint(u64::from(*iterations));
                }
                Some(Scored::Ranked { index, error }) => {
                    w.str("rank");
                    w.map(2);
                    w.str("index");
                    w.str(index);
                    w.str("error");
                    rmp::encode::write_f64(w.0, *error).expect("a Vec");
                }
                None => {}
            }
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
    visited.places += q.places.get();
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
