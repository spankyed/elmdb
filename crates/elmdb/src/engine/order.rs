//! A string's place in the order a value index keeps (`docs/roadmap/string-order.md`): its sort key, the bytes the
//! schema's collator orders it by, as far as the key it is filed under has room for them.
//!
//! Sort keys compare by their bytes as the collator compares their strings, and have no bound a key could hold, so
//! one is cut as it is made: the collator hands its bytes to a sink that keeps the first `room` of them and refuses
//! the next, which stops it there. A cut key is exactly `room` bytes, the first of the whole key, and a whole one is
//! no longer; so of two strings filed under `(sort key, cut)`, the lesser key is the lesser string, but for two whose
//! keys were cut and are the same bytes, which differ past the cut if they differ.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use icu_collator::{CollationKeySink, CollatorBorrowed};
use rmpv::Value;
use sha2::{Digest, Sha256};

use crate::error::{Error, ErrorCode, Result};
use crate::format::key::Part;

use super::unit::{Read, Txn};
use super::{keys, values};

/// A sort key as far as it was kept, and whether more of it was cut off
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SortKey {
    pub bytes: Vec<u8>,
    pub cut: bool,
}

/// Keeps a sort key's first bytes, as many as its room, and refuses the byte after them
struct Kept {
    bytes: Vec<u8>,
    room: usize,
}

/// That a sort key had more bytes than its room
struct Full;

impl CollationKeySink for Kept {
    type Error = Full;
    type State = ();
    type Output = ();

    fn write(&mut self, (): &mut (), bytes: &[u8]) -> std::result::Result<(), Full> {
        let left = self.room - self.bytes.len();
        match bytes.len() <= left {
            true => {
                self.bytes.extend_from_slice(bytes);
                Ok(())
            }
            false => {
                self.bytes.extend_from_slice(&bytes[..left]);
                Err(Full)
            }
        }
    }

    fn finish(&mut self, (): ()) -> std::result::Result<(), Full> {
        Ok(())
    }
}

/// The sort key `collator` gives `string`, cut at `room` bytes
pub fn sort_key(collator: &CollatorBorrowed<'static>, string: &str, room: usize) -> SortKey {
    let mut kept = Kept { bytes: Vec::with_capacity(room.min(2 * string.len() + 8)), room };
    let cut = collator.write_sort_key_to(string, &mut kept).is_err();
    SortKey { bytes: kept.bytes, cut }
}

/// What says which collator made a sort key: the stamp an order's keys are filed under. Sort keys of one stamp
/// compare with each other, and one of another stamp may not, since a sort key is the collator's output and changes
/// with its data, its code and how it is asked. So a stamp is everything a key's bytes follow from: the collation,
/// how the key is made, each crate the collator is built from at the version it is built at, and, should one of those
/// be patched under its version, the keys the running collator gives some strings
pub type Stamp = [u8; 32];

/// The layout of an order's keys and how a sort key is made: the collator's options and the sink that cuts it,
/// which strings have a key of their own (`placed`), the room a sort key has (`keys::order_room`) and what a key
/// holds. Raised with any change to one of them: keys of another layout are then another stamp's, and built again
const LAYOUT: u32 = 1;

/// The most bytes a string takes in a key that holds it after its sort key: its tag, as many bytes as the longest
/// string a value index has an entry for, and its end
pub const PLACED_PART: usize = crate::engine::values::MAX_INDEXED_STRING + 2;

/// Whether a string has a key of its own in an order, holding it: one whose part of a key is no more than
/// `PLACED_PART`, a zero byte taking two (`format::key`). Any other is filed by its entity's place: a string a value
/// index has no entry for (`values::long`), and one of fewer bytes that its zero bytes make as long
pub fn placed(string: &str) -> bool {
    string.len() + string.bytes().filter(|byte| *byte == 0).count() + 2 <= PLACED_PART
}

/// The crates a collator's code and data are, each at the version this elmdb is built with: every ICU crate the
/// lockfile holds, which a test holds this list to
const CRATES: [(&str, &str); 11] = [
    ("icu_collator", "2.4.0"),
    ("icu_collator_data", "2.4.0"),
    ("icu_collections", "2.4.0"),
    ("icu_locale_core", "2.4.0"),
    ("icu_locale_fallback", "2.4.0"),
    ("icu_locale_fallback_data", "2.4.0"),
    ("icu_normalizer", "2.4.0"),
    ("icu_normalizer_data", "2.4.0"),
    ("icu_properties", "2.4.0"),
    ("icu_properties_data", "2.4.0"),
    ("icu_provider", "2.4.0"),
];

/// Strings whose sort keys a stamp takes as the running collator makes them: letters with and without case and
/// accents, composed and combining, digits, punctuation and a space, and several scripts, a few characters of each.
/// They are a sample of the collator's data, and catch a change to it that no version records
const PROBES: [&str; 24] = [
    "", "a", "A", "b", "z", "ab", "a b", "a-b", "é", "e\u{301}", "ö", "å", "ß", "ı", "İ", "10", "9", "α", "я", "あ",
    "カ", "中", "한", "ก",
];

/// The stamp of the keys this elmdb files for a schema's `collation`, whose sort keys `collator` makes
fn stamp_of(collation: &str, collator: &CollatorBorrowed<'static>) -> Stamp {
    let mut hash = Sha256::new();
    // Each part ended, so no two lists of parts are the same bytes
    let mut part = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    };
    part(collation.as_bytes());
    part(&LAYOUT.to_be_bytes());
    for (name, version) in CRATES {
        part(name.as_bytes());
        part(version.as_bytes());
    }
    for probe in PROBES {
        part(&sort_key(collator, probe, usize::MAX).bytes);
    }
    #[cfg(test)]
    part(&[FORGED.with(std::cell::Cell::get)]);
    hash.finalize().into()
}

/// The stamp of the keys this elmdb files for a schema's `collation`
pub fn stamp(collation: &str) -> Result<Stamp> {
    Ok(running(collation)?.stamp)
}

/// A collation's collator and the stamp of the keys it makes
pub struct Running {
    pub collator: CollatorBorrowed<'static>,
    pub stamp: Stamp,
}

thread_local! {
    /// The collators this thread has made, each with its stamp, by collation: one is made once and kept, a stamp
    /// taking some dozens of sort keys to work out. A thread's own, so no write waits on another's for one
    static RUNNING: RefCell<HashMap<String, Rc<Running>>> = RefCell::new(HashMap::new());
    /// What a test adds to every stamp this thread works out, to stand for an elmdb of another collator
    #[cfg(test)]
    static FORGED: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// The collator of a schema's `collation` and its stamp, as this elmdb makes them
pub fn running(collation: &str) -> Result<Rc<Running>> {
    #[cfg(test)]
    let named = format!("{collation}\u{0}{}", FORGED.with(std::cell::Cell::get));
    #[cfg(not(test))]
    let named = collation.to_owned();
    if let Some(found) = RUNNING.with(|running| running.borrow().get(&named).cloned()) {
        return Ok(found);
    }
    let collator = super::collator(collation)?;
    let made = Rc::new(Running { stamp: stamp_of(collation, &collator), collator });
    RUNNING.with(|running| running.borrow_mut().insert(named, Rc::clone(&made)));
    Ok(made)
}

/// What a test's thread forges its stamps with, and sets them to: the builder a database starts takes its opener's
#[cfg(test)]
pub fn forged() -> u8 {
    FORGED.with(std::cell::Cell::get)
}

/// Makes every stamp this thread works out from here on another elmdb's, by `with`; 0 is this elmdb's own
#[cfg(test)]
pub fn forge(with: u8) {
    FORGED.with(|forged| forged.set(with));
}

/// An order's marker (`d/xob`): the stamp its keys were filed under, the build they are under, and whether that
/// build holds every key the index's entries ask for. A read trusts an order whose marker is its own stamp's and
/// whole; a write keeps one of its own stamp, and ends one of another's by deleting the marker
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub stamp: Stamp,
    pub build: u64,
    pub whole: bool,
}

impl Marker {
    pub fn encode(&self) -> Vec<u8> {
        values::encode(&Value::Array(vec![self.stamp.to_vec().into(), self.build.into(), self.whole.into()]))
    }

    fn decode(bytes: &[u8]) -> Result<Marker> {
        let unread = || Error::new(ErrorCode::Corrupted, "An order's marker doesn't read");
        let stored = values::decode(bytes)?;
        let parts = stored.as_array().ok_or_else(unread)?;
        let (Some(Value::Binary(stamp)), Some(build), Some(whole)) =
            (parts.first(), parts.get(1).and_then(Value::as_u64), parts.get(2).and_then(Value::as_bool))
        else {
            return Err(unread());
        };
        Ok(Marker { stamp: stamp.as_slice().try_into().map_err(|_| unread())?, build, whole })
    }
}

/// The marker of an index's order, when it has one. One that doesn't read is no elmdb's this one knows: it is
/// answered as a marker of no stamp, which no read trusts, a write deletes and the builder builds over, so bytes
/// another layout or a damaged page left there cost an order a build and nothing else
pub fn marker(r: &impl Read, entity_type: &str, index: &str) -> Result<Option<Marker>> {
    let unread = Marker { stamp: [0; 32], build: 0, whole: false };
    Ok(r.get(&keys::order_marker(entity_type, index))?.map(|stored| Marker::decode(&stored).unwrap_or(unread)))
}

/// Whether an index's order holds a key under a build other than `build`: what a build that ended before its
/// last unit left, or one that was replaced. Two seeks, the keys being in the order of their builds
pub fn stale(r: &impl Read, entity_type: &str, index: &str, build: u64) -> Result<bool> {
    let (low, high) = crate::format::key::prefix_range(&keys::order_of(entity_type, index));
    let (first, past) = crate::format::key::prefix_range(&keys::order_range(entity_type, index, build, &[]));
    let mut found = false;
    for (from, to) in [(&low, &first), (&past, &high)] {
        r.visit_within(from, to, |_, _| {
            found = true;
            false
        })?;
    }
    Ok(found)
}

/// The build a read takes an index's order from: its marker's, when that is this elmdb's stamp and whole
pub fn readable(r: &impl Read, entity_type: &str, index: &str) -> Result<Option<u64>> {
    let stamp = stamp(&r.schema().collation)?;
    Ok(marker(r, entity_type, index)?.filter(|marker| marker.stamp == stamp && marker.whole).map(|marker| marker.build))
}

/// The build a write files an index's order under, as it files or takes out a string there: its marker's, when
/// that is this elmdb's stamp, whole or not. A marker of another stamp is deleted, which ends that build, since
/// what the other files for this write is its layout's and unknown here; and then, as with no marker, nothing is
/// filed and the order is owed a build, which the unit's commit asks for
pub fn filing(t: &mut Txn<'_, '_>, entity_type: &str, index: &str) -> Result<Option<u64>> {
    let stamp = stamp(&t.schema.collation)?;
    let own = keys::order_marker(entity_type, index);
    match marker(t, entity_type, index)? {
        Some(marker) if marker.stamp == stamp => Ok(Some(marker.build)),
        found => {
            if found.is_some() {
                t.del(&own)?;
            }
            t.state.orders_owed.insert((entity_type.to_owned(), index.to_owned()));
            Ok(None)
        }
    }
}

/// What files an index's order under a build: the build, and the collator its sort keys are made by
#[derive(Clone)]
pub struct Filing {
    pub build: u64,
    pub running: Rc<Running>,
}

impl Filing {
    /// The key the order has for the string an entry is for, given the entry's own key (`string_key`): none for an
    /// entry of anything else, and for a key that is no entry's
    pub fn of_entry(&self, own: &[u8]) -> Option<Vec<u8>> {
        if !own.starts_with(&crate::format::Prefix::Index.key().to_vec()) {
            return None;
        }
        let parts = crate::format::key::decode(own).ok()?;
        let (Some(Part::String(entity_type)), Some(Part::String(index))) = (parts.get(1), parts.get(2)) else {
            return None;
        };
        string_key(&self.running.collator, entity_type, index, self.build, parts.get(3..)?)
    }

    /// The keys the order has for the entity at `place`, one for each string it holds that has no key of its own
    /// (`entities::Filed::unplaced`)
    pub fn of_places(
        &self,
        entity_type: &str,
        index: &str,
        unplaced: &[(Vec<Part>, String)],
        place: u64,
    ) -> impl Iterator<Item = Vec<u8>> {
        let key = move |(leading, string): &(Vec<Part>, String)| {
            place_key(&self.running.collator, entity_type, index, self.build, leading, string, place)
        };
        unplaced.iter().filter_map(key)
    }
}

/// What files an index's order where a read trusts it, under its marker's build: none when the marker is another
/// stamp's, isn't whole, or isn't there
pub fn trusted(r: &impl Read, entity_type: &str, index: &str) -> Result<Option<Filing>> {
    match readable(r, entity_type, index)? {
        Some(build) => Ok(Some(Filing { build, running: running(&r.schema().collation)? })),
        None => Ok(None),
    }
}

/// Marks an index's order whole under a build begun by this unit, which files every key of it: what `rebuild()`
/// does for an index it builds. The build is the sequence the unit commits as, so no two of a database share one
pub fn begin_whole(t: &mut Txn<'_, '_>, entity_type: &str, index: &str) -> Result<Filing> {
    let running = running(&t.schema.collation)?;
    let build = super::mirror::stored_sequence(t)? + 1;
    let marker = Marker { stamp: running.stamp, build, whole: true };
    t.put(&keys::order_marker(entity_type, index), &marker.encode())?;
    Ok(Filing { build, running })
}

/// Where a build of an index's order has got to, which the builder holds between its units
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Not begun: the marker says what there is to do
    Begin,
    /// The index's entries read as far as the key `after`, each string's key put where there was none; with
    /// `past`, as far as that place among those the key holds, an entry of a string with no key of its own having
    /// one for each
    Entries { build: u64, after: Option<Vec<u8>>, past: Option<u64> },
    /// And so the entities holding long strings, by the keys their places are under
    Long { build: u64, after: Option<Vec<u8>>, past: Option<u64> },
    /// Whole. What is left under any other build is deleted
    Sweep { build: u64 },
    /// Nothing more to do
    Done,
    /// The order has no marker of this elmdb's, and this build may not begin one yet
    PutOff,
}

/// Whether a type's index is one whose order is kept: a value index the schema declares, built
fn kept(r: &impl Read, entity_type: &str, index: &str) -> Result<Option<super::entities::ValueIndex>> {
    let declared = super::entities::value_indexes(r.schema(), entity_type).into_iter().find(|i| i.name == index);
    match declared.filter(super::entities::ValueIndex::accounts) {
        Some(found) if super::derive::is_built(r, entity_type, index)? => Ok(Some(found)),
        _ => Ok(None),
    }
}

/// One unit of the build of an index's order (`docs/roadmap/string-order.md`, A stale sort key is never read),
/// answering where the build is after it, and whether it began one.
///
/// A build is begun under a marker of this elmdb's stamp, not whole, and its number the sequence this unit commits
/// as. It goes on with one it finds so, from the index's first key, and leaves one that is whole. Each unit after
/// the first holds the marker to the stamp and the build it began under and begins again when it is another: a
/// write of another stamp ended it, or a `rebuild()` replaced it. Writes of this stamp keep the keys either side
/// of where the build is, so it puts only a key that isn't there. With `may_begin` false it begins none
pub fn build_unit(
    t: &mut Txn<'_, '_>,
    entity_type: &str,
    index: &str,
    mut step: Step,
    may_begin: bool,
    limit: usize,
) -> Result<(Step, bool)> {
    // As far as the unit's share of the work goes: a part that ended short of it leaves the next its turn, so a
    // small index's order is built in one unit
    let mut began = false;
    loop {
        let before = std::mem::discriminant(&step);
        let (reached, begun) = build_step(t, entity_type, index, step, may_begin, limit)?;
        began |= begun;
        step = reached;
        if matches!(step, Step::Done | Step::PutOff) || std::mem::discriminant(&step) == before {
            return Ok((step, began));
        }
    }
}

/// One part of `build_unit`: the build's beginning, `limit` of the index's entries or of the keys its long strings
/// are under, or `limit` keys of other builds deleted
fn build_step(
    t: &mut Txn<'_, '_>,
    entity_type: &str,
    index: &str,
    step: Step,
    may_begin: bool,
    limit: usize,
) -> Result<(Step, bool)> {
    let Some(found) = kept(t, entity_type, index)? else { return Ok((Step::Done, false)) };
    let running = running(&t.schema.collation)?;
    let held = marker(t, entity_type, index)?;
    // The marker a unit of the build is under: this stamp's, its build, and whole only once it has been made so
    let under = |build: u64, whole: bool| {
        held.as_ref().is_some_and(|m| m.stamp == running.stamp && m.build == build && m.whole == whole)
    };
    let put = |t: &mut Txn<'_, '_>, key: Option<Vec<u8>>| -> Result<()> {
        match key {
            Some(key) if !t.contains(&key)? => t.put(&key, &[]),
            _ => Ok(()),
        }
    };
    match step {
        Step::Done | Step::PutOff => Ok((step, false)),
        Step::Begin => match held {
            Some(m) if m.stamp == running.stamp && m.whole => Ok((Step::Sweep { build: m.build }, false)),
            Some(m) if m.stamp == running.stamp => {
                Ok((Step::Entries { build: m.build, after: None, past: None }, false))
            }
            _ if !may_begin => Ok((Step::PutOff, false)),
            _ => {
                let build = super::mirror::stored_sequence(t)? + 1;
                let begun = Marker { stamp: running.stamp, build, whole: false };
                t.put(&keys::order_marker(entity_type, index), &begun.encode())?;
                Ok((Step::Entries { build, after: None, past: None }, true))
            }
        },
        Step::Entries { build, after, past } => {
            if !under(build, false) {
                return Ok((Step::Begin, false));
            }
            let filing = Filing { build, running };
            let entries = keys::index_range(entity_type, index, &[]);
            let read = keys_from(t, &entries, None, &after, past, limit)?;
            let mut left = limit;
            for (own, _) in &read {
                // An entry part done has its own key already, and its places as far as `past`
                let resumed = if after.as_ref() == Some(own) { past } else { None };
                if resumed.is_none() {
                    put(t, filing.of_entry(own))?;
                    left = left.saturating_sub(1);
                }
                // A string with no key of its own has one for each entity its entry holds
                let parts = crate::format::key::decode(own)?;
                if let Some((Part::String(string), leading)) = parts.get(3..).and_then(<[Part]>::split_last)
                    && !placed(string)
                {
                    let places = places_past(t, own, resumed, left.max(1))?;
                    for place in &places {
                        let key =
                            place_key(&filing.running.collator, entity_type, index, build, leading, string, *place);
                        put(t, key)?;
                    }
                    left = left.saturating_sub(places.len());
                    if left == 0
                        && let Some(last) = places.last()
                    {
                        return Ok((Step::Entries { build, after: Some(own.clone()), past: Some(*last) }, false));
                    }
                }
                if left == 0 {
                    return Ok((Step::Entries { build, after: Some(own.clone()), past: None }, false));
                }
            }
            match read.len() < limit {
                true => Ok((Step::Long { build, after: None, past: None }, false)),
                false => {
                    Ok((Step::Entries { build, after: read.last().map(|(key, _)| key.clone()), past: None }, false))
                }
            }
        }
        Step::Long { build, after, past } => {
            if !under(build, false) {
                return Ok((Step::Begin, false));
            }
            let filing = Filing { build, running };
            // The keys the entities holding a long string are under: the range's own, for an index on one field,
            // and one for each tuple under it
            let range = keys::index_others(crate::format::Prefix::IndexLong, entity_type, index, &[]);
            let read = keys_from(t, &range, Some(&range), &after, past, limit)?;
            let last = found.fields.last().map(String::as_str).unwrap_or_default();
            let mut left = limit;
            for (own, _) in &read {
                let resumed = if after.as_ref() == Some(own) { past } else { None };
                let parts = crate::format::key::decode(own)?;
                let leading = parts.get(3..).unwrap_or_default();
                let asked = left.max(1);
                let places = places_past(t, own, resumed, asked)?;
                for place in &places {
                    let Some((id, _)) = super::entities::at_place(t, entity_type, *place)? else { continue };
                    if let Some(Value::String(string)) = super::entities::get(t, &id, last)?
                        && let Some(string) = string.as_str()
                    {
                        let key =
                            place_key(&filing.running.collator, entity_type, index, build, leading, string, *place);
                        put(t, key)?;
                    }
                }
                left = left.saturating_sub(places.len().max(1));
                // As many places as were asked for may not be all the key holds: the next unit goes on past them
                if places.len() == asked {
                    return Ok((Step::Long { build, after: Some(own.clone()), past: places.last().copied() }, false));
                }
                if left == 0 {
                    return Ok((Step::Long { build, after: Some(own.clone()), past: None }, false));
                }
            }
            if read.len() >= limit {
                return Ok((Step::Long { build, after: read.last().map(|(key, _)| key.clone()), past: None }, false));
            }
            let whole = Marker { stamp: filing.running.stamp, build, whole: true };
            t.put(&keys::order_marker(entity_type, index), &whole.encode())?;
            Ok((Step::Sweep { build }, false))
        }
        Step::Sweep { build } => {
            if !under(build, true) {
                return Ok((Step::Begin, false));
            }
            // What is under a build before the marker's, and after it: no order's
            let (low, high) = crate::format::key::prefix_range(&keys::order_of(entity_type, index));
            let (first, past) = crate::format::key::prefix_range(&keys::order_range(entity_type, index, build, &[]));
            let mut stale: Vec<Vec<u8>> = Vec::new();
            for (from, to) in [(&low, &first), (&past, &high)] {
                t.visit_within(from, to, |key, _| {
                    stale.push(key.to_vec());
                    stale.len() < limit
                })?;
            }
            for key in &stale {
                t.del(key)?;
            }
            Ok((if stale.len() < limit { Step::Done } else { Step::Sweep { build } }, false))
        }
    }
}

/// The keys under `under` a build's step reads next, `limit` of them at most, each with what it holds: from the
/// first, past the key `after`, or from `after` itself when the step left off among its places (`past`). With
/// `own`, the range's own key is the first, which no key under it is
fn keys_from(
    t: &Txn<'_, '_>,
    under: &[u8],
    own: Option<&Vec<u8>>,
    after: &Option<Vec<u8>>,
    past: Option<u64>,
    limit: usize,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut read: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let from = match (after, past) {
        (None, _) => {
            if let Some(own) = own {
                read.extend(t.get(own)?.map(|stored| (own.clone(), stored)));
            }
            under.to_vec()
        }
        // The range's own key again, which a walk of what is under it doesn't come to
        (Some(key), Some(_)) if own == Some(key) => {
            read.extend(t.get(key)?.map(|stored| (key.clone(), stored)));
            under.to_vec()
        }
        (Some(key), Some(_)) => key.clone(),
        (Some(key), None) => {
            let mut next = key.clone();
            next.push(0);
            next
        }
    };
    if read.len() < limit {
        t.visit_from(under, &from, |key, stored| {
            read.push((key.to_vec(), stored.to_vec()));
            read.len() < limit
        })?;
    }
    Ok(read)
}

/// The places filed under the key `own` past the place `past`, or all of them, `limit` at most, in order
fn places_past(t: &Txn<'_, '_>, own: &[u8], past: Option<u64>, limit: usize) -> Result<Vec<u64>> {
    let from = match past {
        Some(place) => match place.checked_add(1) {
            Some(from) => from,
            None => return Ok(Vec::new()),
        },
        None => 0,
    };
    let mut sought = super::text::postings::Sought::new(super::places::term(own.to_vec()));
    let mut places = Vec::new();
    super::places::from(t, &mut sought, from, &mut |place| {
        places.push(place);
        Ok(places.len() < limit)
    })?;
    Ok(places)
}

/// Where a unit of a build would leave it having written nothing, when that is so, read without a unit: an index
/// that keeps no order, an order whole with nothing under another build, and one that may not begin yet. A unit
/// commits whatever it wrote, and a commit is heard by every handle on the database: one that says nothing is
/// not made
pub fn settled(r: &impl Read, entity_type: &str, index: &str, step: &Step, may_begin: bool) -> Result<Option<Step>> {
    if kept(r, entity_type, index)?.is_none() {
        return Ok(Some(Step::Done));
    }
    let stamp = stamp(&r.schema().collation)?;
    let held = marker(r, entity_type, index)?.filter(|marker| marker.stamp == stamp);
    Ok(match (step, held) {
        (Step::Done | Step::PutOff, _) => Some(step.clone()),
        (Step::Begin, None) if !may_begin => Some(Step::PutOff),
        (Step::Begin, Some(marker)) if marker.whole && !stale(r, entity_type, index, marker.build)? => Some(Step::Done),
        (Step::Sweep { build }, Some(marker))
            if marker.whole && marker.build == *build && !stale(r, entity_type, index, marker.build)? =>
        {
            Some(Step::Done)
        }
        _ => None,
    })
}

/// Removes an index's order: its keys under every build, and its marker
pub fn clear(t: &mut Txn<'_, '_>, entity_type: &str, index: &str) -> Result<()> {
    t.clear_prefix(&keys::order_of(entity_type, index))?;
    let marker = keys::order_marker(entity_type, index);
    if t.contains(&marker)? {
        t.del(&marker)?;
    }
    Ok(())
}

/// The key that says an index has an entry for the string its `parts` end with, under the parts before it: none
/// for parts ending with no string, for a string filed by its entities' places (`placed`), and under parts that
/// leave a key no room
pub fn string_key(
    collator: &CollatorBorrowed<'static>,
    entity_type: &str,
    index: &str,
    build: u64,
    parts: &[Part],
) -> Option<Vec<u8>> {
    let (Part::String(string), leading) = parts.split_last()? else { return None };
    if !placed(string) {
        return None;
    }
    let room = keys::order_room(entity_type, index, leading)?;
    Some(keys::order_string(entity_type, index, build, leading, &sort_key(collator, string, room), string))
}

/// The key that says the entity at `place` holds `string`, one with no key of its own, under these leading parts:
/// none under parts that leave a key no room
pub fn place_key(
    collator: &CollatorBorrowed<'static>,
    entity_type: &str,
    index: &str,
    build: u64,
    leading: &[Part],
    string: &str,
    place: u64,
) -> Option<Vec<u8>> {
    let room = keys::order_room(entity_type, index, leading)?;
    Some(keys::order_place(entity_type, index, build, leading, &sort_key(collator, string, room), place))
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use proptest::prelude::*;

    use super::*;
    use crate::engine::{collator, keys};
    use crate::format::key::Part;

    /// The collations the cut was read in (`docs/roadmap/string-order.md`, the third reading): the root's, locales
    /// that tailor it, one that reads its accents backwards, and ones that order digits as numbers, capitals first, a
    /// phone book and pinyin
    const COLLATIONS: [&str; 11] = [
        "root",
        "sv",
        "fr-CA",
        "ja",
        "th",
        "lt",
        "ko",
        "de-u-kn",
        "en-u-kf-upper",
        "de-u-co-phonebk",
        "zh-u-co-pinyin",
    ];

    /// Strings of several scripts, with capitals, accents composed and combining, digits, punctuation, characters
    /// the collation ignores, and one whose sort key is some forty bytes
    fn string() -> impl Strategy<Value = String> {
        let character = prop_oneof![
            6 => prop::sample::select(vec!['a', 'b', 'e', 'z', 'A', 'B', 'E', 'Z', ' ', '-', '1', '2', '9', '0']),
            3 => prop::sample::select(vec!['é', 'è', 'ö', 'å', 'ß', 'ñ', 'İ', 'ı', '\u{301}', '\u{308}']),
            2 => prop::sample::select(vec!['α', 'Ω', 'я', 'Ж', 'あ', 'カ', '中', '한', 'ก', 'ไ', '🙂']),
            1 => prop::sample::select(vec!['\u{200b}', '\u{ad}', '\u{fdfa}', '\u{1}']),
        ];
        prop::collection::vec(character, 0..40).prop_map(|characters| characters.into_iter().collect())
    }

    proptest! {
        #[test]
        fn sort_keys_order_as_the_collator_does_and_a_cut_one_is_the_whole_ones_first_bytes(
            collation in prop::sample::select(COLLATIONS.to_vec()),
            strings in prop::collection::vec(string(), 2..12),
            room in prop_oneof![Just(1usize), Just(8), Just(64), Just(200), 1usize..120],
        ) {
            let collator = collator(collation).unwrap();
            let whole: Vec<SortKey> = strings.iter().map(|s| sort_key(&collator, s, usize::MAX)).collect();
            let cut: Vec<SortKey> = strings.iter().map(|s| sort_key(&collator, s, room)).collect();
            for (at, string) in strings.iter().enumerate() {
                // No key holds a zero byte, which a key part ends at; a whole key says so; and a cut one is the
                // room's length exactly, the whole key's first bytes
                prop_assert!(!whole[at].cut && !whole[at].bytes.contains(&0), "{string:?}");
                let kept = whole[at].bytes.len().min(room);
                prop_assert_eq!(&cut[at].bytes, &whole[at].bytes[..kept], "{:?} in {}", string, room);
                prop_assert_eq!(cut[at].cut, whole[at].bytes.len() > room, "{:?} in {}", string, room);
            }
            for a in 0..strings.len() {
                for b in 0..strings.len() {
                    let order = collator.compare(&strings[a], &strings[b]);
                    // The whole keys' bytes order two strings as the collator does, and are the same bytes for
                    // two it holds equal
                    prop_assert_eq!(whole[a].cmp(&whole[b]), order, "{:?} and {:?}", strings[a], strings[b]);
                    // Keys cut at one room do too, the cut after the bytes, except two cut to the same bytes,
                    // whose strings differ past the cut if at all
                    let one_run = cut[a] == cut[b] && cut[a].cut;
                    if !one_run {
                        prop_assert_eq!(cut[a].cmp(&cut[b]), order, "{:?} and {:?} in {}", strings[a], strings[b], room);
                    } else {
                        prop_assert!(order != Ordering::Equal || whole[a] == whole[b]);
                    }
                }
            }
        }
    }

    /// A part before an index's last: a string, a number, a boolean or null
    fn leading_part() -> impl Strategy<Value = Part> {
        prop_oneof![
            "[a-z\\x00é]{0,90}".prop_map(Part::String),
            (-1e9f64..1e9).prop_map(Part::Number),
            any::<bool>().prop_map(Part::Bool),
            Just(Part::Null),
        ]
    }

    proptest! {
        #[test]
        fn no_key_of_an_order_is_longer_than_a_key_may_be_and_one_fills_it(
            entity_type in "[A-Za-z]{1,70}",
            index in "[A-Za-z]{1,70}",
            leading in prop::collection::vec(leading_part(), 0..3),
            build in 0u64..(1 << 53),
            place in 0u64..(1 << 53),
            string in "[a-zé\\x00]{0,256}",
            filled in 0usize..600,
            cut in any::<bool>(),
        ) {
            let most = crate::engine::values::MAX_KEY_BYTES;
            let Some(room) = keys::order_room(&entity_type, &index, &leading) else {
                // Under these names and values a key with no byte of a sort key wouldn't fit its longest string
                let begins = keys::order_range(&entity_type, &index, build, &leading).len();
                prop_assert!(begins + 2 + 1 + PLACED_PART > most);
                return Ok(());
            };
            // A sort key holds no zero byte, and is no longer than its room
            let sort_key = SortKey { bytes: vec![0x2b; filled.min(room)], cut };
            let of_place = keys::order_place(&entity_type, &index, build, &leading, &sort_key, place);
            prop_assert!(of_place.len() <= most);
            if placed(&string) {
                let of_string = keys::order_string(&entity_type, &index, build, &leading, &sort_key, &string);
                prop_assert!(of_string.len() <= most, "{} bytes", of_string.len());
            }
            // The longest string with a key of its own, after a sort key that fills its room, is a key of exactly
            // the most bytes: the room is all there is, and no more
            let full = SortKey { bytes: vec![0x2b; room], cut };
            let longest = keys::order_string(&entity_type, &index, build, &leading, &full, &"a".repeat(256));
            prop_assert_eq!(longest.len(), most);
        }
    }

    #[test]
    fn an_order_s_keys_are_in_the_order_of_their_build_sort_key_cut_and_string() {
        let key = |bytes: &[u8], cut: bool| SortKey { bytes: bytes.to_vec(), cut };
        let string = |build, leading: &[Part], sort_key: &SortKey, string: &str| {
            keys::order_string("Note", "name", build, leading, sort_key, string)
        };
        let ordered = [
            // A place is before a string under one sort key, and strings are in the order of their bytes
            keys::order_place("Note", "name", 7, &[], &key(&[0x2b], false), 9),
            string(7, &[], &key(&[0x2b], false), "a"),
            string(7, &[], &key(&[0x2b], false), "a\0"),
            string(7, &[], &key(&[0x2b], false), "b"),
            // A whole key before the same bytes cut, and a key that goes on after either
            string(7, &[], &key(&[0x2b], true), "a"),
            string(7, &[], &key(&[0x2b, 0x05], false), "a"),
            string(7, &[], &key(&[0x2c], false), "a"),
            // And a later build after them all
            string(8, &[], &key(&[0x2b], false), "a"),
        ];
        for pair in ordered.windows(2) {
            assert!(pair[0] < pair[1], "{:?} and {:?}", pair[0], pair[1]);
        }
        // Each is under its build's and its index's keys, and the marker is no key of the order's
        assert!(ordered[..7].iter().all(|one| one.starts_with(&keys::order_range("Note", "name", 7, &[]))));
        assert!(ordered.iter().all(|one| one.starts_with(&keys::order_of("Note", "name"))));
        // Under the values an index's fields before the last hold, the values first: every key of one tuple before
        // any of the next, whatever their sort keys
        let (done, open) = ([Part::String("done".into())], [Part::String("open".into())]);
        let under_done = string(7, &done, &key(&[0x2c], true), "z");
        let under_open = string(7, &open, &key(&[0x2b], false), "a");
        assert!(under_done < under_open);
        assert!(under_done.starts_with(&keys::order_range("Note", "name", 7, &done)));
        assert!(!under_done.starts_with(&keys::order_range("Note", "name", 7, &open)));
        assert!(!keys::order_marker("Note", "name").starts_with(&keys::order_of("Note", "name")));
    }

    #[test]
    fn a_stamp_is_one_collation_s_and_holds_the_crates_the_lockfile_does() {
        // The same for the same collation, and another's for another, a locale whose order is the root's among them:
        // the collation is in the stamp, not only what it makes of the probes
        assert_eq!(stamp("root").unwrap(), stamp("root").unwrap());
        let stamps: Vec<Stamp> = ["root", "sv", "en", "de-u-co-phonebk"].iter().map(|c| stamp(c).unwrap()).collect();
        for (at, one) in stamps.iter().enumerate() {
            assert!(stamps[at + 1..].iter().all(|other| other != one), "{at}");
        }
        assert!(stamp("no locale").is_err());
        // Every ICU crate the workspace builds with, at its version: a crate updated or added is a stamp to change
        let lock = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"));
        let mut locked: Vec<(&str, &str)> = Vec::new();
        let mut lines = lock.lines();
        while let Some(line) = lines.next() {
            if let Some(name) = line.strip_prefix("name = \"icu_").and_then(|rest| rest.strip_suffix('"')) {
                let version = lines.next().unwrap().strip_prefix("version = \"").unwrap().trim_end_matches('"');
                locked.push((&line[8..8 + 4 + name.len()], version));
            }
        }
        assert_eq!(locked, CRATES);
    }

    #[test]
    fn a_sort_key_s_bytes_are_those_the_stamp_was_made_for() {
        // The root collation's keys for a few strings, as this stamp's collator makes them. A change here is a
        // change to what is stored under the stamp: `LAYOUT` or `CRATES` moves with it, or the stored keys are stale
        let root = collator("root").unwrap();
        let key = |string: &str| sort_key(&root, string, usize::MAX).bytes;
        assert_eq!(key("a"), [0x2b, 0x01, 0x05, 0x01, 0x05]);
        assert_eq!(key("Ab"), [0x2b, 0x2d, 0x01, 0x06, 0x01, 0xdc, 0x05]);
        assert_eq!(key("é"), [0x33, 0x01, 0x45, 0x88, 0x01, 0x06]);
    }

    #[test]
    fn a_key_is_cut_where_its_room_ends_and_whole_where_it_fits() {
        let collator = collator("root").unwrap();
        let whole = sort_key(&collator, "Banana", usize::MAX);
        assert!(!whole.cut && whole.bytes.len() > 4);
        // A room of its length holds it whole, one byte less cuts it, and no room keeps nothing of it
        assert_eq!(sort_key(&collator, "Banana", whole.bytes.len()), whole);
        let short = sort_key(&collator, "Banana", whole.bytes.len() - 1);
        assert_eq!(short, SortKey { bytes: whole.bytes[..whole.bytes.len() - 1].to_vec(), cut: true });
        assert_eq!(sort_key(&collator, "Banana", 0), SortKey { bytes: Vec::new(), cut: true });
        // The key of a whole string before the same bytes cut: a string that is another's beginning is the lesser
        assert!(whole < SortKey { bytes: whole.bytes.clone(), cut: true });
    }
}
