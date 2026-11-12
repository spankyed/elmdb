//! A term's places (`docs/architecture/search.md`, Text search as built): the entities of a type holding a term of a
//! text index, each with the term's count in each of the index's fields.
//!
//! A term held by no more than `capacity` entities keeps them in its own key, `d/ft`. One held by more keeps them in
//! blocks, `d/fb term start`, and its own key says only that. A block covers a range of places whose size is a power
//! of two and whose start is a multiple of its size: the largest such range holding no more than `capacity` of the
//! term's entities, where the range twice its size holds more. So what a term's keys hold follows from the entities
//! holding it and from nothing else, whatever order they were filed in, which is what lets `check()` derive the same
//! bytes; a write that files or unfiles an entity rewrites one block, and at times splits one or joins two.
//!
//! - an entry: the place, as its distance from the entry before it (the first's from where its block starts), then
//!   the counts
//! - the counts: for an index of one field, its count; for one of several, a bit a field for those holding the term,
//!   lowest field first, then their counts in field order
//! - a term's own key: how many entries, then the entries from place 0; or a single 0, for a term in blocks
//! - a block: the power of two its range's size is, how many entries, then the entries from its start
//!
//! Numbers are LEB128 varints.

use roaring::{RoaringBitmap, RoaringTreemap};

use crate::error::{Error, ErrorCode, Result};

use super::super::keys;
use super::super::unit::{Read, Txn};

/// The most entities a term's own key, or one block, holds: at two to four bytes an entry, a block is a kilobyte at
/// most, small beside a page of any size LMDB uses
pub const BLOCK_PLACES: usize = 256;

fn corrupt() -> Error {
    Error::new(ErrorCode::Corrupted, "An index's places don't read: rebuild() derives them again")
}

fn put_varint(out: &mut Vec<u8>, n: u64) {
    // Seven bits a byte, the lowest first, so ten bytes at most: the last is what is left of a u64 past 63 bits
    for shift in (0..64).step_by(7) {
        let rest = n >> shift;
        if rest < 0x80 {
            return out.push(rest as u8);
        }
        out.push(rest as u8 | 0x80);
    }
}

/// The varint `bytes` starts with, and `bytes` moved past it
#[inline]
fn varint(bytes: &mut &[u8]) -> Result<u64> {
    // Nearly every number here is under 128, a byte: a distance between places, a count
    if let Some((&byte, rest)) = bytes.split_first()
        && byte < 0x80
    {
        *bytes = rest;
        return Ok(u64::from(byte));
    }
    let mut n = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or_else(corrupt)?;
        *bytes = rest;
        n |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(n);
        }
    }
    Err(corrupt())
}

/// A term's counts in an entity as an entry holds them, from where each field holds it: `(field, positions)` in field
/// order, as a document has them
pub fn counts(places: &[(u32, Vec<u32>)], fields: usize) -> Vec<u8> {
    let mut out = Vec::new();
    if fields > 1 {
        out.resize(fields.div_ceil(8), 0);
        for (field, _) in places {
            out[*field as usize / 8] |= 1 << (field % 8);
        }
    }
    for (_, positions) in places {
        put_varint(&mut out, positions.len() as u64);
    }
    out
}

/// How many bytes of `bytes` the counts it starts with take
fn counts_len(bytes: &[u8], fields: usize) -> Result<usize> {
    let mut rest = bytes;
    each_count(&mut rest, fields, |_, _| {})?;
    Ok(bytes.len() - rest.len())
}

/// Each field holding the term and its count there, in field order, from the counts `bytes` starts with, and `bytes`
/// moved past them
fn each_count(bytes: &mut &[u8], fields: usize, mut each: impl FnMut(u32, u64)) -> Result<()> {
    // A list of places alone, which has no fields: an entry is its place and nothing more
    if fields == 0 {
        return Ok(());
    }
    if fields <= 1 {
        each(0, varint(bytes)?);
        return Ok(());
    }
    let (held, rest) = bytes.split_at_checked(fields.div_ceil(8)).ok_or_else(corrupt)?;
    *bytes = rest;
    for field in 0..fields {
        if held[field / 8] & (1 << (field % 8)) != 0 {
            each(field as u32, varint(bytes)?);
        }
    }
    Ok(())
}

/// Each field holding the term and its count there, in field order, from an entry's counts
pub fn counted(mut counts: &[u8], fields: usize, each: impl FnMut(u32, u64)) -> Result<()> {
    each_count(&mut counts, fields, each)
}

/// An entry's counts added up, each at its field's weight among `weights`, in field order: a term's frequency in an
/// entity as BM25F counts it
#[inline]
pub fn weighted(counts: &[u8], weights: &[f64]) -> Result<f64> {
    // An index of one field, and a count a byte holds, as nearly every one is
    if let ([weight], [count]) = (weights, counts)
        && *count < 0x80
    {
        return Ok(weight * f64::from(*count));
    }
    let mut sum = 0.0;
    counted(counts, weights.len(), |field, count| sum += weights[field as usize] * count as f64)?;
    Ok(sum)
}

/// An entry as a block is read: its place, and its counts as stored
type Entry<'a> = (u64, &'a [u8]);

/// The entries of one key as the key says they are: how many, the place the first is counted from, and the last
/// place one of them may be at
#[derive(Clone, Copy)]
struct Held {
    count: u64,
    start: u64,
    end: u64,
}

impl Held {
    /// A term's own key's, which may be at any place
    fn own(count: u64) -> Self {
        Held { count, start: 0, end: u64::MAX }
    }
}

/// Each entry `bytes` holds as it reads, and as `held` says: each at a later place than the one before and none past
/// the last it may be at, as many as it counts. That is what every reader and every layout of them takes them to be,
/// and what is refused here is rewritten by no write. An entry is a byte at least, so there are no more of them than
/// bytes, and the reading ends there whatever the bytes hold
fn each_entry(bytes: &[u8], held: Held, fields: usize) -> impl Iterator<Item = Result<Entry<'_>>> {
    let mut entries = Entries { place: held.start, rest: bytes, fields };
    let mut last = None;
    let mut left = held.count;
    (0..=bytes.len()).map_while(move |_| {
        if entries.rest.is_empty() {
            // Fewer than counted is refused once, where they end
            return (std::mem::take(&mut left) > 0).then(|| Err(corrupt()));
        }
        Some((|| {
            let entry = entries.read()?;
            if last.is_some_and(|last| entry.0 <= last) || entry.0 > held.end {
                return Err(corrupt());
            }
            left = left.checked_sub(1).ok_or_else(corrupt)?;
            last = Some(entry.0);
            Ok(entry)
        })())
    })
}

/// The entries `bytes` holds, as `held` says they are
fn entries(bytes: &[u8], held: Held, fields: usize) -> Result<Vec<Entry<'_>>> {
    each_entry(bytes, held, fields).collect()
}

fn write_entries<C: AsRef<[u8]>>(out: &mut Vec<u8>, start: u64, list: &[(u64, C)]) {
    let mut last = start;
    for (place, counts) in list {
        put_varint(out, place - last);
        out.extend_from_slice(counts.as_ref());
        last = *place;
    }
}

/// What a term's own key holds when its entries are in it
fn own<C: AsRef<[u8]>>(list: &[(u64, C)]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, list.len() as u64);
    write_entries(&mut out, 0, list);
    out
}

/// What a term's own key holds when its entries are in blocks
const IN_BLOCKS: [u8; 1] = [0];

/// What a block holds
fn block_value<C: AsRef<[u8]>>(level: u32, start: u64, list: &[(u64, C)]) -> Vec<u8> {
    let mut out = vec![level as u8];
    put_varint(&mut out, list.len() as u64);
    write_entries(&mut out, start, list);
    out
}

/// The entries a term's own key holds and what it says of them, or none for a term in blocks
fn own_entries(stored: &[u8]) -> Result<Option<(Held, &[u8])>> {
    let mut rest = stored;
    Ok(match varint(&mut rest)? {
        0 => None,
        count => Some((Held::own(count), rest)),
    })
}

/// Hands `each` the places a list's own key holds as `stored`, in order, until it answers false: whether it went
/// through them all. None for a list in blocks, which holds none of its own, and which `Sought` reads: as it reads
/// one whose own key holds them, to the same places, so answering none for every list changes no answer
pub fn each_own(stored: &[u8], each: &mut dyn FnMut(u64) -> Result<bool>) -> Result<Option<bool>> {
    // mutant-skip: replace each_own -> Result<Option<bool>> with Ok\(None\)
    let Some((held, bytes)) = own_entries(stored)? else { return Ok(None) };
    for entry in each_entry(bytes, held, 0) {
        if !each(entry?.0)? {
            return Ok(Some(false));
        }
    }
    Ok(Some(true))
}

/// How many places the range of a block at `start` covers, two to the `level`: one a place can be in, that starts at
/// a multiple of its size
fn range(start: u64, level: u32) -> Result<u64> {
    1u64.checked_shl(level).filter(|size| start.is_multiple_of(*size)).ok_or_else(corrupt)
}

/// The power of two the smallest range holding two different places is the size of, among ranges that start at a
/// multiple of their size
fn shared(a: u64, b: u64) -> u32 {
    64 - (a ^ b).leading_zeros()
}

/// Whether two places are in one range of size two to the `level`
fn together(a: u64, b: u64, level: u32) -> bool {
    shared(a, b) <= level
}

/// Where the range of size two to the `level` that holds `place` starts
fn floor(place: u64, level: u32) -> u64 {
    place >> level << level
}

/// The range a block holding `place` takes, as the power of two its size is, when the nearest places in other
/// blocks are `before` and `after`: the largest that holds neither. None when there is no other block
fn room(place: u64, before: Option<u64>, after: Option<u64>) -> Option<u32> {
    before.into_iter().chain(after).map(|other| shared(place, other) - 1).min()
}

/// The first block of `waiting`, the entries not yet in a block, in place order: where it starts, the power of two
/// its range's size is, and how many of them it takes. `last` is the place of the last entry already in a block.
/// The block is the largest range holding the first entry that holds neither `last` nor more than `capacity`
fn first_block<C>(waiting: &[(u64, C)], last: Option<u64>, capacity: usize) -> (u64, u32, usize) {
    let first = waiting[0].0;
    let beyond = waiting.get(capacity).map(|(place, _)| *place);
    // With no entry before them and none past a block's worth, they are every entry of a term few enough hold for its
    // own key, which no caller lays out in blocks
    let level = room(first, last, beyond).expect("a block's entries follow others, or more than a block's worth do");
    let take = waiting.partition_point(|(place, _)| together(first, *place, level));
    (floor(first, level), level, take)
}

/// A block as it is laid out: where it starts, the power of two its range's size is, and its entries
type Laid<'a, C> = (u64, u32, &'a [(u64, C)]);

/// The blocks a term's entries are in, in place order. For entries too many for the term's own key
fn blocks<C>(list: &[(u64, C)], capacity: usize) -> Vec<Laid<'_, C>> {
    let mut out = Vec::new();
    let mut rest = list;
    let mut last = None;
    // A block holds an entry at least, so there are no more of them than entries
    for _ in 0..list.len() {
        if rest.is_empty() {
            break;
        }
        let (start, level, take) = first_block(rest, last, capacity);
        let (held, after) = rest.split_at(take);
        last = held.last().map(|(place, _)| *place);
        out.push((start, level, held));
        rest = after;
    }
    out
}

/// A term of a text index: its own key, the key its blocks are under, how many fields its index has, and how many
/// entities a key holds
pub struct Term {
    own: Vec<u8>,
    blocks: Vec<u8>,
    fields: usize,
    capacity: usize,
}

impl Term {
    pub fn new(entity_type: &str, index: &str, fields: usize, term: &str) -> Self {
        Term {
            own: keys::text_term(entity_type, index, term),
            blocks: keys::text_blocks(entity_type, index, term),
            fields,
            capacity: BLOCK_PLACES,
        }
    }

    /// A list of places with no counts, laid out as a term's are: its own key, and the key its blocks are under.
    /// What a value index holds for one value (`engine/places.rs`)
    pub fn places(own: Vec<u8>, blocks: Vec<u8>) -> Self {
        Term { own, blocks, fields: 0, capacity: BLOCK_PLACES }
    }

    fn block(&self, start: u64) -> Vec<u8> {
        keys::place_under(&self.blocks, start)
    }
}

/// A block as stored
struct Block {
    key: Vec<u8>,
    stored: Vec<u8>,
    start: u64,
    level: u32,
    /// How many places its range covers
    size: u64,
}

impl Block {
    fn read(key: &[u8], stored: &[u8]) -> Result<Block> {
        let start = keys::place_of(key).ok_or_else(corrupt)?;
        let level = u32::from(*stored.first().ok_or_else(corrupt)?);
        let size = range(start, level)?;
        Ok(Block { key: key.to_vec(), stored: stored.to_vec(), start, level, size })
    }

    fn holds(&self, place: u64) -> bool {
        together(self.start, place, self.level)
    }

    fn entries(&self, fields: usize) -> Result<Vec<Entry<'_>>> {
        let mut rest = &self.stored[1..];
        let count = varint(&mut rest)?;
        // A range starts at a multiple of its size, so its last place is no more than a u64 holds
        entries(rest, Held { count, start: self.start, end: self.start + (self.size - 1) }, fields)
    }
}

/// The term's last block starting at `place` or before it, which holds `place` if any does
fn block_upto(t: &Txn<'_, '_>, term: &Term, place: u64) -> Result<Option<Block>> {
    t.last_upto(&term.blocks, &term.block(place))?.map(|(key, stored)| Block::read(&key, &stored)).transpose()
}

/// The term's first block starting at `place` or after it
fn block_from(t: &Txn<'_, '_>, term: &Term, place: u64) -> Result<Option<Block>> {
    let mut found = None;
    t.visit_from(&term.blocks, &term.block(place), |key, stored| {
        found = Some(Block::read(key, stored));
        false
    })?;
    found.transpose()
}

/// Puts `counts` at `place` among `list`, in place of what it had there
fn set<'a>(list: &mut Vec<Entry<'a>>, place: u64, counts: &'a [u8]) {
    match list.binary_search_by_key(&place, |(at, _)| *at) {
        Ok(at) => list[at].1 = counts,
        Err(at) => list.insert(at, (place, counts)),
    }
}

/// Files the entity at `place` under the term with these counts, in place of those it had there
pub fn put(t: &mut Txn<'_, '_>, term: &Term, place: u64, counts: &[u8]) -> Result<()> {
    let Some(stored) = t.get(&term.own)? else {
        return t.put_over(&term.own, &own(&[(place, counts)]), None);
    };
    if let Some((held, stored_entries)) = own_entries(&stored)? {
        let mut list = entries(stored_entries, held, term.fields)?;
        set(&mut list, place, counts);
        if list.len() <= term.capacity {
            return t.put_over(&term.own, &own(&list), Some(&stored));
        }
        for (start, level, held) in blocks(&list, term.capacity) {
            t.put(&term.block(start), &block_value(level, start, held))?;
        }
        return t.put_over(&term.own, &IN_BLOCKS, Some(&stored));
    }
    let before = block_upto(t, term, place)?;
    if let Some(block) = before.as_ref().filter(|block| block.holds(place)) {
        let mut list = block.entries(term.fields)?;
        set(&mut list, place, counts);
        if list.len() <= term.capacity {
            return t.put_over(&block.key, &block_value(block.level, block.start, &list), Some(&block.stored));
        }
        // One more than a block holds: two blocks, in the halves of the smallest range that parts its entries
        let mut kept = false;
        for (start, level, held) in blocks(&list, term.capacity) {
            let value = block_value(level, start, held);
            if start == block.start {
                kept = true;
                t.put_over(&block.key, &value, Some(&block.stored))?;
            } else {
                t.put(&term.block(start), &value)?;
            }
        }
        return if kept { Ok(()) } else { t.del_over(&block.key, Some(&block.stored)) };
    }
    // No block holds the place: one of its own, as large as leaves the blocks either side alone
    let after = block_from(t, term, place)?;
    let level = room(place, before.map(|block| block.start), after.map(|block| block.start)).ok_or_else(corrupt)?;
    let start = floor(place, level);
    t.put(&term.block(start), &block_value(level, start, &[(place, counts)]))
}

/// Takes the entity at `place` out of the term's
pub fn remove(t: &mut Txn<'_, '_>, term: &Term, place: u64) -> Result<()> {
    let Some(stored) = t.get(&term.own)? else { return Ok(()) };
    if let Some((held, stored_entries)) = own_entries(&stored)? {
        let mut list = entries(stored_entries, held, term.fields)?;
        let Ok(at) = list.binary_search_by_key(&place, |(at, _)| *at) else { return Ok(()) };
        list.remove(at);
        return if list.is_empty() {
            t.del_over(&term.own, Some(&stored))
        } else {
            t.put_over(&term.own, &own(&list), Some(&stored))
        };
    }
    let Some(block) = block_upto(t, term, place)?.filter(|block| block.holds(place)) else { return Ok(()) };
    let mut list = block.entries(term.fields)?;
    let Ok(at) = list.binary_search_by_key(&place, |(at, _)| *at) else { return Ok(()) };
    list.remove(at);
    // The other half of the range twice this block's size: one block, when it and what is left here are no more
    // than a block holds, and then the two are one block of the whole range
    let size = block.size;
    let beside_start = block.start ^ size;
    let beside = block_from(t, term, beside_start)?.filter(|other| together(beside_start, other.start, block.level));
    let beside_list = match &beside {
        None => Some(Vec::new()),
        Some(other) if other.start == beside_start && other.level == block.level => Some(other.entries(term.fields)?),
        // Blocks smaller than the half: more than a block's worth
        Some(_) => None,
    };
    let Some(beside_list) = beside_list.filter(|beside_list| list.len() + beside_list.len() <= term.capacity) else {
        return if list.is_empty() {
            t.del_over(&block.key, Some(&block.stored))
        } else {
            t.put_over(&block.key, &block_value(block.level, block.start, &list), Some(&block.stored))
        };
    };
    let low = block.start.min(beside_start);
    let mut all = list;
    all.extend(beside_list);
    all.sort_by_key(|(place, _)| *place);
    // The blocks either side of the two, which the one they become grows towards: its range is the largest that
    // holds neither, since every range between holds only these entries
    let before = match low.checked_sub(1) {
        Some(place) => block_upto(t, term, place)?,
        None => None,
    };
    let after = match size.checked_mul(2).and_then(|both| low.checked_add(both)) {
        Some(place) => block_from(t, term, place)?,
        None => None,
    };
    t.del_over(&block.key, Some(&block.stored))?;
    if let Some(other) = &beside {
        t.del_over(&other.key, Some(&other.stored))?;
    }
    match room(low, before.map(|block| block.start), after.map(|block| block.start)) {
        Some(level) => {
            let start = floor(low, level);
            t.put(&term.block(start), &block_value(level, start, &all))
        }
        // No other block: every entry the term has, few enough for its own key again
        None => t.put_over(&term.own, &own(&all), Some(&stored)),
    }
}

/// Emits what a term's keys hold, from its entries handed over in place order: the term's own key, and its blocks
/// when they are more than the key holds. No more than a block's worth and one are held at a time
pub struct Filing {
    term: Term,
    waiting: Vec<(u64, Vec<u8>)>,
    last: Option<u64>,
}

impl Filing {
    pub fn new(term: Term) -> Self {
        Filing { term, waiting: Vec::new(), last: None }
    }

    pub fn entry(
        &mut self,
        place: u64,
        counts: Vec<u8>,
        emit: &mut impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        self.waiting.push((place, counts));
        if self.waiting.len() > self.term.capacity { self.block(emit) } else { Ok(()) }
    }

    fn block(&mut self, emit: &mut impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>) -> Result<()> {
        let (start, level, take) = first_block(&self.waiting, self.last, self.term.capacity);
        emit(self.term.block(start), block_value(level, start, &self.waiting[..take]))?;
        self.last = Some(self.waiting[take - 1].0);
        self.waiting.drain(..take);
        Ok(())
    }

    pub fn finish(mut self, emit: &mut impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>) -> Result<()> {
        if self.last.is_none() {
            return match self.waiting.is_empty() {
                true => Ok(()),
                false => emit(self.term.own, own(&self.waiting)),
            };
        }
        while !self.waiting.is_empty() {
            self.block(emit)?;
        }
        emit(self.term.own, IN_BLOCKS.to_vec())
    }
}

/// How many entities hold a term, from what its own key holds
pub fn held(r: &impl Read, term: &Term, stored: &[u8]) -> Result<u64> {
    held_as(r, term, stored, None)
}

/// `held`, read as a probe and no further than `upto` takes: what a count that decides how a plan is read asks.
/// Blocks are counted whole, so the answer may pass `upto` by what the last one read holds. With it, whether
/// that is every entity holding the term: true for a term whose own key holds them, and for one in blocks false
/// when the count came to `upto`, where it stopped
pub fn held_probed(r: &impl Read, term: &Term, stored: &[u8], upto: u64) -> Result<(u64, bool)> {
    let mut rest = stored;
    let own = varint(&mut rest)? > 0;
    let held = held_as(r, term, stored, Some(upto))?;
    Ok((held, own || held < upto))
}

fn held_as(r: &impl Read, term: &Term, stored: &[u8], probe: Option<u64>) -> Result<u64> {
    let mut rest = stored;
    let count = varint(&mut rest)?;
    if count > 0 {
        return Ok(count);
    }
    let mut total = 0u64;
    let mut failed = None;
    let each = |_: &[u8], block: &[u8]| {
        let mut rest = block.get(1..).unwrap_or_default();
        match varint(&mut rest) {
            Ok(count) => match total.checked_add(count) {
                Some(sum) => total = sum,
                None => failed = Some(corrupt()),
            },
            Err(error) => failed = Some(error),
        }
        failed.is_none() && probe.is_none_or(|upto| total < upto)
    };
    let (low, high) = crate::format::key::prefix_range(&term.blocks);
    if probe.is_some() {
        r.probe_visit_within(&low, &high, each)?;
    } else {
        r.visit_within(&low, &high, each)?;
    }
    match failed {
        Some(error) => Err(error),
        None => Ok(total),
    }
}

/// A term's places read where they are asked for, a key at a time: its own key's, or one block's. A read that wants
/// the first few of a term many entities hold reads the block they are in and no other, where `read` takes in every
/// block the term has
pub struct Sought {
    term: Term,
    /// Whether the term's own key was read, and what it said: no entity, its entries, or that they are in blocks
    own: Option<Own>,
    /// The places of the key last read, in order, and the places it answers for, from the first to the last: none
    /// but these are among them
    run: Vec<u64>,
    covers: Option<(u64, u64)>,
    /// The key the term's blocks begin after
    low: Vec<u8>,
}

/// Reads into `run` the places of the block `stored` at `key`, and answers the first and the last place of its
/// range. A block that doesn't hold `holding` is left unread, and answers none
fn block_into(
    key: &[u8],
    stored: &[u8],
    fields: usize,
    holding: Option<u64>,
    run: &mut Vec<u64>,
) -> Result<Option<(u64, u64)>> {
    let start = keys::place_of(key).ok_or_else(corrupt)?;
    let (&level, mut rest) = stored.split_first().ok_or_else(corrupt)?;
    let level = u32::from(level);
    let size = range(start, level)?;
    if holding.is_some_and(|place| !together(start, place, level)) {
        return Ok(None);
    }
    let count = varint(&mut rest)?;
    // A range starts at a multiple of its size, so its last place is no more than a u64 holds
    let end = start + (size - 1);
    run.clear();
    for entry in each_entry(rest, Held { count, start, end }, fields) {
        run.push(entry?.0);
    }
    Ok(Some((start, end)))
}

#[derive(Clone, Copy, PartialEq)]
enum Own {
    Nothing,
    Entries,
    Blocks,
}

impl Sought {
    pub fn new(term: Term) -> Self {
        let (low, _) = crate::format::key::prefix_range(&term.blocks);
        Sought { term, own: None, run: Vec::new(), covers: None, low }
    }

    /// A term whose own key holds `stored`, which a walk of the keys it is among has read already
    pub fn stored(term: Term, stored: &[u8]) -> Result<Self> {
        let mut sought = Sought::new(term);
        sought.own = Some(sought.read_own(stored)?);
        Ok(sought)
    }

    /// Takes in what the term's own key holds
    fn read_own(&mut self, stored: &[u8]) -> Result<Own> {
        Ok(match own_entries(stored)? {
            Some((held, bytes)) => {
                self.run = places_of(bytes, held, self.term.fields)?;
                self.covers = Some((0, u64::MAX));
                Own::Entries
            }
            None => Own::Blocks,
        })
    }

    /// The first place the term's entities are at that is `place` or after it. The store is read only for a place
    /// the key last read doesn't answer for: `read` says whether it was
    pub fn seek(&mut self, r: &impl Read, mut place: u64, read: &mut bool) -> Result<Option<u64>> {
        if self.own.is_none() {
            *read = true;
            self.own = Some(match r.get(&self.term.own)? {
                None => Own::Nothing,
                Some(stored) => self.read_own(&stored)?,
            });
        }
        if self.own == Some(Own::Nothing) {
            return Ok(None);
        }
        // A block is read at most twice over: the one the place is in, and when that holds none so late, the next
        let asked = place;
        for _ in 0..3 {
            if let Some((first, last)) = self.covers
                && (first..=last).contains(&place)
            {
                let at = self.run.partition_point(|held| *held < place);
                if let Some(found) = self.run.get(at) {
                    return Ok(Some(*found));
                }
                // No place of this key's is so late, and the places after its last are another's
                match last.checked_add(1) {
                    Some(after) => place = after,
                    None => return Ok(None),
                }
            }
            *read = true;
            let found = self.read(r, place)?;
            // What was read answers for the place asked for too: none lies between it and the place read from
            if let Some((first, last)) = self.covers {
                self.covers = Some((first.min(asked), last));
            }
            if !found {
                return Ok(None);
            }
        }
        Err(corrupt())
    }

    /// Reads the block that answers for `place`: the one it is in, or with none the first after it, which answers
    /// for every place from `place` to its last. False when the term has no block so late
    fn read(&mut self, r: &impl Read, place: u64) -> Result<bool> {
        // Until a block is read whole, the places held answer for nothing
        self.covers = None;
        let mut past = self.term.block(place);
        let from = past.len();
        past.push(0);
        let fields = self.term.fields;
        let run = &mut self.run;
        let mut found = None;
        r.visit_back_within(&self.low, &past, |key, stored| {
            found = Some(block_into(key, stored, fields, Some(place), run));
            false
        })?;
        if let Some(covers) = found.transpose()?.flatten() {
            self.covers = Some(covers);
            return Ok(true);
        }
        let mut found = None;
        r.visit_from(&self.term.blocks, &past[..from], |key, stored| {
            found = Some(block_into(key, stored, fields, None, run));
            false
        })?;
        match found.transpose()?.flatten() {
            Some((_, last)) => {
                self.covers = Some((place, last));
                Ok(true)
            }
            None => {
                self.run.clear();
                self.covers = Some((place, u64::MAX));
                Ok(false)
            }
        }
    }
}

/// The places of the entries `bytes` holds, as `held` says they are
fn places_of(bytes: &[u8], held: Held, fields: usize) -> Result<Vec<u64>> {
    each_entry(bytes, held, fields).map(|entry| entry.map(|(place, _)| place)).collect()
}

/// A term's entities as a search reads them: their places as a set, and each's counts
pub struct Postings {
    pub places: RoaringTreemap,
    /// Each run of entries as stored, a block's or the term's own key's: the place its first is counted from, and
    /// where it starts in `bytes`
    runs: Vec<(u64, usize)>,
    bytes: Vec<u8>,
    fields: usize,
}

/// The entities holding a term, or none when no entity does
pub fn read(r: &impl Read, term: &Term) -> Result<Option<Postings>> {
    match r.get(&term.own)? {
        Some(stored) => read_stored(r, term, &stored).map(Some),
        None => Ok(None),
    }
}

/// The entities holding a term, from what its own key holds
pub fn read_stored(r: &impl Read, term: &Term, stored: &[u8]) -> Result<Postings> {
    let mut reading = Reading { parts: Vec::new(), runs: Vec::new(), bytes: Vec::new(), fields: term.fields };
    if let Some((held, stored)) = own_entries(stored)? {
        reading.run(held, false, stored)?;
        return Ok(reading.done());
    }
    let mut failed = None;
    r.visit(&term.blocks, |key, stored| {
        let run = (|| {
            let start = keys::place_of(key).ok_or_else(corrupt)?;
            let level = u32::from(*stored.first().ok_or_else(corrupt)?);
            let mut rest = &stored[1..];
            let count = varint(&mut rest)?;
            // A block starts at a multiple of its range's size, which is what lets its places be read as a range
            let size = range(start, level)?;
            // A block whose every place is held needs no entry read to say which
            reading.run(Held { count, start, end: start + (size - 1) }, size == count, rest)
        })();
        failed = run.err();
        failed.is_none()
    })?;
    match failed {
        Some(error) => Err(error),
        None => Ok(reading.done()),
    }
}

/// A term's entities as they are read: the set of their places a part at a time, each the places sharing their upper
/// 32 bits, which is how the set keeps them and so where a place past the last is added without a search for its part
struct Reading {
    parts: Vec<(u32, RoaringBitmap)>,
    runs: Vec<(u64, usize)>,
    bytes: Vec<u8>,
    fields: usize,
}

impl Reading {
    /// The part holding `place`, which is the last part or one after it
    fn part(&mut self, place: u64) -> Result<&mut RoaringBitmap> {
        let upper = (place >> 32) as u32;
        match self.parts.last().map(|(held, _)| *held) {
            Some(held) if held > upper => return Err(corrupt()),
            Some(held) if held == upper => {}
            _ => self.parts.push((upper, RoaringBitmap::new())),
        }
        Ok(&mut self.parts.last_mut().expect("a part was found or added").1)
    }

    /// Takes in a run of entries as stored, which are what `held` says; `full` when they are every place it may hold
    fn run(&mut self, held: Held, full: bool, stored: &[u8]) -> Result<()> {
        let start = held.start;
        if full {
            // The places of one part: a range that reaches past it is no block's
            let from = start as u32;
            let span = u32::try_from(held.end - start).map_err(|_| corrupt())?;
            let last = from.checked_add(span).ok_or_else(corrupt)?;
            self.part(start)?.insert_range(from..=last);
        } else {
            for entry in each_entry(stored, held, self.fields) {
                let (place, _) = entry?;
                self.part(place)?.try_push(place as u32).map_err(|_| corrupt())?;
            }
        }
        self.runs.push((start, self.bytes.len()));
        self.bytes.extend_from_slice(stored);
        Ok(())
    }

    fn done(self) -> Postings {
        Postings {
            places: RoaringTreemap::from_bitmaps(self.parts),
            runs: self.runs,
            bytes: self.bytes,
            fields: self.fields,
        }
    }
}

impl Postings {
    /// The entries of the run at `at`
    fn run_at(&self, at: usize) -> Entries<'_> {
        let (start, from) = self.runs[at];
        let to = self.runs.get(at + 1).map_or(self.bytes.len(), |(_, from)| *from);
        Entries { place: start, rest: &self.bytes[from..to], fields: self.fields }
    }

    /// The counts of the entity at `place`, which the term may not hold
    pub fn at(&self, place: u64) -> Result<Option<&[u8]>> {
        let Some(run) = self.runs.partition_point(|(start, _)| *start <= place).checked_sub(1) else {
            return Ok(None);
        };
        for entry in self.run_at(run) {
            let (at, counts) = entry?;
            if at >= place {
                return Ok((at == place).then_some(counts));
            }
        }
        Ok(None)
    }

    /// The term's entities from the first, for a reader that asks for places in order
    pub fn walk(&self) -> Walk<'_> {
        Walk { postings: self, run: 0, entries: Entries { place: 0, rest: &[], fields: self.fields }, next: None }
    }

    /// Every entity holding the term, with its counts, in place order, a run ending at an entry that doesn't read. An
    /// entry is a byte at least, so there are no more of them than bytes, and they end there whatever a run reads as
    pub fn entries(&self) -> impl Iterator<Item = Result<(u64, &[u8])>> {
        (0..self.runs.len()).flat_map(|at| self.run_at(at)).take(self.bytes.len())
    }
}

/// A run's entries, in place order
struct Entries<'a> {
    place: u64,
    rest: &'a [u8],
    fields: usize,
}

impl<'a> Entries<'a> {
    /// The next entry, of a run that has one
    #[inline]
    fn read(&mut self) -> Result<Entry<'a>> {
        self.place = self.place.checked_add(varint(&mut self.rest)?).ok_or_else(corrupt)?;
        let length = match self.rest.first() {
            // mutant-skip: replace match guard self.fields == 1 && \*count < 0x80 with false in Entries<'a>::read
            // An index of one field, and a count a byte holds: the length the arm below finds for it, without the call
            Some(count) if self.fields == 1 && *count < 0x80 => 1,
            _ => counts_len(self.rest, self.fields)?,
        };
        let (counts, rest) = self.rest.split_at(length);
        self.rest = rest;
        Ok((self.place, counts))
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Entry<'a>>;

    /// The next entry, or that it doesn't read, which ends the run: `read_stored` read each as it took the run in,
    /// but for a block that says its every place is held, whose entries are first read here
    fn next(&mut self) -> Option<Result<Entry<'a>>> {
        if self.rest.is_empty() {
            return None;
        }
        let entry = self.read();
        if entry.is_err() {
            self.rest = &[];
        }
        Some(entry)
    }
}

/// A term's entities read in place order
pub struct Walk<'a> {
    postings: &'a Postings,
    /// The run after the one being read
    run: usize,
    entries: Entries<'a>,
    next: Option<Entry<'a>>,
}

impl<'a> Walk<'a> {
    /// The counts of the entity at `place`, when the term holds it. Each place asked for is after the last
    #[inline]
    pub fn to(&mut self, place: u64) -> Result<Option<&'a [u8]>> {
        // A run starting at the place or before it is where the place is if the term holds it: the last such run is
        // found among those not yet read, and the runs before it are passed unread
        let ahead = self.postings.runs[self.run..].partition_point(|(start, _)| *start <= place);
        if ahead > 0 {
            self.run += ahead;
            self.entries = self.postings.run_at(self.run - 1);
            self.next = None;
        }
        // The run being read is now the only one that can hold the place, so the place is not held once it ends
        loop {
            if let Some((at, counts)) = self.next
                && at >= place
            {
                return Ok((at == place).then_some(counts));
            }
            match self.entries.next() {
                Some(entry) => self.next = Some(entry?),
                None => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests;
