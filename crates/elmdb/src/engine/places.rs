//! A value index's places in the store (`docs/architecture/format.md`, Keys): under each tuple of values the index
//! files an entity under, the places of the entities filed there, laid out as a text index lays out a term's
//! (`text/postings.rs`). The tuple's own key, `d/x type index values`, holds the places while they are few and says
//! that they are in blocks once they are not; a block, `d/xp type index values start`, holds those of a range of
//! places. The entities an index files besides its entries are kept the same way under `d/xw` and `d/xu`.
//!
//! An entry holds no id: the entity at a place is the one its type's rows name there (`d/e`, `d/t`).

use roaring::RoaringTreemap;

use crate::error::Result;

use super::keys;
use super::text::postings::{self, Term};
use super::unit::{Read, Txn};

/// The list of places whose own key is `own`
pub fn term(own: Vec<u8>) -> Term {
    let blocks = keys::index_blocks(&own);
    Term::places(own, blocks)
}

/// Files the entity at `place` under the key `own`
pub fn file(t: &mut Txn<'_, '_>, own: Vec<u8>, place: u64) -> Result<()> {
    postings::put(t, &term(own), place, &[])
}

/// Takes the entity at `place` out from under the key `own`
pub fn unfile(t: &mut Txn<'_, '_>, own: Vec<u8>, place: u64) -> Result<()> {
    postings::remove(t, &term(own), place)
}

/// The places filed under `own`, whose own key holds `stored`
pub fn held(r: &impl Read, own: &[u8], stored: &[u8]) -> Result<RoaringTreemap> {
    Ok(postings::read_stored(r, &term(own.to_vec()), stored)?.places)
}

/// The places filed under `own`: none when it has no key
pub fn under(r: &impl Read, own: &[u8]) -> Result<RoaringTreemap> {
    match r.get(own)? {
        Some(stored) => held(r, own, &stored),
        None => Ok(RoaringTreemap::new()),
    }
}

/// How many places are filed under `own`, whose own key holds `stored`
pub fn count(r: &impl Read, own: &[u8], stored: &[u8]) -> Result<u64> {
    postings::held(r, &term(own.to_vec()), stored)
}

/// `count`, read as a probe and no further than `upto` takes: what a count that decides how a plan is read asks.
/// Places in blocks are counted a block at a time, so the answer may pass `upto` by what the last block read holds.
/// With it, whether that is every place under the key: false when the count came to `upto`, where it stopped
pub fn count_probed(r: &impl Read, own: &[u8], stored: &[u8], upto: u64) -> Result<(u64, bool)> {
    postings::held_probed(r, &term(own.to_vec()), stored, upto)
}

/// Hands `each` the places `sought` holds from `from` on, in order: a key of them is read as the walk comes to
/// it, so one stopped early reads no further. False when `each` stopped it
pub fn from(
    r: &impl Read,
    sought: &mut postings::Sought,
    from: u64,
    each: &mut dyn FnMut(u64) -> Result<bool>,
) -> Result<bool> {
    let mut next = from;
    while let Some(place) = sought.seek(r, next, &mut false)? {
        if !each(place)? {
            return Ok(false);
        }
        match place.checked_add(1) {
            Some(after) => next = after,
            None => break,
        }
    }
    Ok(true)
}

/// Hands `each` every place filed under each own key from `from` up to `to`, with the key: the keys in order, or from
/// the last when `back`, and a key's places in order either way. False when `each` stopped it
pub fn visit(
    r: &impl Read,
    from: &[u8],
    to: &[u8],
    back: bool,
    each: &mut dyn FnMut(&[u8], u64) -> Result<bool>,
) -> Result<bool> {
    let mut outcome = Ok(true);
    let mut key_by_key = |own: &[u8], stored: &[u8]| {
        let step = (|| {
            let mut sought = postings::Sought::stored(term(own.to_vec()), stored)?;
            self::from(r, &mut sought, 0, &mut |place| each(own, place))
        })();
        match step {
            Ok(true) => true,
            other => {
                outcome = other;
                false
            }
        }
    };
    if back {
        r.visit_back_within(from, to, &mut key_by_key)?;
    } else {
        r.visit_within(from, to, &mut key_by_key)?;
    }
    outcome
}

/// What a value index's keys hold, from each place it files under each key, handed over in any order: the places
/// are sorted on disk, where one handed over twice under a key is kept once, and each key's are laid out in turn
pub struct Sorting {
    rows: super::derive::sorted::Scratch,
}

impl Sorting {
    pub fn new() -> Result<Self> {
        Ok(Sorting { rows: super::derive::sorted::Scratch::new()? })
    }

    /// Files `place` under the key `own`
    pub fn entry(&mut self, own: Vec<u8>, place: u64) -> Result<()> {
        use crate::format::key::{Part, encode};
        // Escaped byte parts keep a key's places together, whatever bytes the key and the place hold
        self.rows.push(encode(&[Part::Bytes(own), Part::Bytes(place.to_be_bytes().to_vec())]), Vec::new())
    }

    /// Emits each key's own key and blocks
    pub fn finish(self, mut emit: impl FnMut(Vec<u8>, Vec<u8>) -> Result<()>) -> Result<()> {
        use crate::error::{Error, ErrorCode};
        use crate::format::key::{Part, decode};
        let unread = || Error::new(ErrorCode::Corrupted, "A sorted value index row doesn't read");
        let mut sorted = self.rows.finish()?;
        let mut filing: Option<(Vec<u8>, postings::Filing)> = None;
        while let Some((order, _)) = sorted.next()? {
            let mut parts = decode(&order)?.into_iter();
            let (Some(Part::Bytes(own)), Some(Part::Bytes(place))) = (parts.next(), parts.next()) else {
                return Err(unread());
            };
            let place = u64::from_be_bytes(place.as_slice().try_into().map_err(|_| unread())?);
            if filing.as_ref().is_none_or(|(of, _)| *of != own) {
                if let Some((_, done)) = filing.take() {
                    done.finish(&mut emit)?;
                }
                filing = Some((own.clone(), postings::Filing::new(term(own))));
            }
            if let Some((_, filing)) = filing.as_mut() {
                filing.entry(place, Vec::new(), &mut emit)?;
            }
        }
        match filing {
            Some((_, done)) => done.finish(&mut emit),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emitted(entries: &[(&[u8], u64)]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut sorting = Sorting::new().unwrap();
        for (own, place) in entries {
            sorting
                .entry(keys::index_range("Task", "byN", &[]).into_iter().chain(own.iter().copied()).collect(), *place)
                .unwrap();
        }
        let mut out = Vec::new();
        sorting
            .finish(|key, value| {
                out.push((key, value));
                Ok(())
            })
            .unwrap();
        out
    }

    #[test]
    fn places_handed_over_in_any_order_are_filed_once_each() {
        let once = emitted(&[(b"a", 3), (b"a", 9), (b"b", 5)]);
        assert_eq!(once.len(), 2, "a key for each of the two");
        // The same places out of order, one of them twice: the same keys holding the same
        assert_eq!(emitted(&[(b"b", 5), (b"a", 9), (b"a", 3), (b"a", 9)]), once);
    }
}
