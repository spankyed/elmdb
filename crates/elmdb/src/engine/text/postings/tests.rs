//! A term's places: that its keys hold what its entities alone make them, however they were filed and unfiled, that a
//! search reads back each entity's counts, and what bytes that don't read as places are refused as.

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::json;

use super::*;
use crate::engine::unit::UnitState;
use crate::format::key::prefix_range;
use crate::schema::Schema;
use crate::store::{Store, StoreOptions};

/// One term's keys, in a database of their own: of an index of `fields` fields whose keys hold `capacity` entities
struct Filed {
    _dir: tempfile::TempDir,
    store: Store,
    schema: Schema,
    fields: usize,
    capacity: usize,
}

impl Filed {
    fn new(fields: usize, capacity: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreOptions { syncs: false, ..Default::default() }).unwrap();
        let schema = Schema::from_json(&json!({ "$schema": crate::schema::SCHEMA_LANGUAGE })).unwrap();
        Filed { _dir: dir, store, schema, fields, capacity }
    }

    fn term(&self) -> Term {
        Term { capacity: self.capacity, ..Term::new("Note", "text", self.fields, "word") }
    }

    /// Runs `work` in a unit of its own, with a savepoint open when `savepoint`
    fn write<T>(&self, savepoint: bool, mut work: impl FnMut(&mut Txn<'_, '_>) -> Result<T>) -> Result<T> {
        self.store.write(
            |txn, db| {
                let mut state = UnitState::new(false);
                if savepoint {
                    state.savepoint();
                }
                work(&mut Txn { txn, db, state: &mut state, schema: &self.schema, now: 0.0 })
            },
            false,
        )
    }

    /// Every key of the term, its own and its blocks', as stored
    fn stored(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let term = self.term();
        self.store
            .read(|txn, db| {
                let mut out = Vec::new();
                if let Some(own) = db.get(txn, &term.own)? {
                    out.push((term.own.clone(), own.to_vec()));
                }
                let (low, high) = prefix_range(&term.blocks);
                let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                for entry in db.range(txn, &range)? {
                    let (key, value) = entry?;
                    out.push((key.to_vec(), value.to_vec()));
                }
                Ok(out)
            })
            .unwrap()
    }

    /// The term's blocks as stored: each's start, the power of two its range's size is, and its places
    fn blocks(&self) -> Vec<(u64, u32, Vec<u64>)> {
        let fields = self.fields;
        self.stored()
            .iter()
            .skip(1)
            .map(|(key, stored)| {
                let block = Block::read(key, stored).unwrap();
                (block.start, block.level, block.entries(fields).unwrap().iter().map(|(place, _)| *place).collect())
            })
            .collect()
    }

    fn snapshot<T>(&self, read: impl FnOnce(&crate::engine::unit::Snapshot<'_, '_>) -> Result<T>) -> Result<T> {
        self.store.read(|txn, db| read(&crate::engine::unit::Snapshot { txn, db, schema: &self.schema }))
    }
}

/// What a term's keys hold when its entities are handed over in place order
fn filed_in_order(term: Term, model: &BTreeMap<u64, Vec<u8>>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut emit = |key: Vec<u8>, value: Vec<u8>| {
        out.push((key, value));
        Ok(())
    };
    let mut filing = Filing::new(term);
    for (place, counts) in model {
        filing.entry(*place, counts.clone(), &mut emit).unwrap();
    }
    filing.finish(&mut emit).unwrap();
    // As `Filed::stored` reads them: the term's own key, then its blocks in place order
    out.sort_by_key(|(key, _)| (key.starts_with(&crate::format::Prefix::TextBlock.key().to_vec()), key.clone()));
    out
}

/// The blocks of places the definition gives: a range holding no more than `capacity` is a block, and one holding
/// more is its two halves'
fn by_halves(places: &[u64], start: u64, level: u32, capacity: usize, out: &mut Vec<(u64, u32, Vec<u64>)>) {
    if places.is_empty() {
        return;
    }
    if places.len() <= capacity {
        return out.push((start, level, places.to_vec()));
    }
    let middle = start + (1 << (level - 1));
    let at = places.partition_point(|place| *place < middle);
    by_halves(&places[..at], start, level - 1, capacity, out);
    by_halves(&places[at..], middle, level - 1, capacity, out);
}

/// Places near each other, places far apart, and a few past where one byte or two hold a distance
fn place() -> impl Strategy<Value = u64> {
    prop_oneof![
        4 => 0..40u64,
        2 => (0..12u64).prop_map(|n| n * 1000 + 3),
        1 => (0..4u64).prop_map(|n| (1 << 40) + n * 77),
    ]
}

/// An entity's counts in an index of `fields` fields: the term once in its first field; or, for `often` 1 or 2, 256 or
/// 300 times in its last field, beside once in its first when the index has more than one. 256 and 300 are two bytes,
/// the first the least such a byte is and one past that
fn counts_for(fields: usize, often: u8) -> Vec<u8> {
    let first = (0, vec![0]);
    let last = (fields as u32 - 1, (0..if often == 1 { 256 } else { 300 }).collect());
    match (often, fields) {
        (0, _) => counts(&[first], fields),
        (_, 1) => counts(&[last], fields),
        _ => counts(&[first, last], fields),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn a_terms_keys_hold_what_its_entities_alone_make_them_however_they_were_filed(
        fields in prop_oneof![Just(1usize), Just(3), Just(9)],
        capacity in 1usize..5,
        ops in prop::collection::vec((place(), prop::option::weighted(0.7, 0u8..3)), 0..70),
    ) {
        let filed = Filed::new(fields, capacity);
        let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        for (place, op) in ops {
            filed.write(false, |t| match op {
                Some(often) => put(t, &filed.term(), place, &counts_for(fields, often)),
                None => remove(t, &filed.term(), place),
            }).unwrap();
            match op {
                Some(often) => model.insert(place, counts_for(fields, often)),
                None => model.remove(&place),
            };
            prop_assert_eq!(filed.stored(), filed_in_order(filed.term(), &model), "after {:?} at {}", op, place);
        }
        // What a search reads of it: the places as a set, each one's counts where it is, and none between them
        filed.snapshot(|r| {
            let Some(postings) = read(r, &filed.term())? else {
                assert!(model.is_empty());
                return Ok(());
            };
            assert_eq!(postings.places.iter().collect::<Vec<_>>(), model.keys().copied().collect::<Vec<_>>());
            let read: Vec<(u64, &[u8])> = postings.entries().collect::<Result<_>>()?;
            assert_eq!(read, model.iter().map(|(place, counts)| (*place, counts.as_slice())).collect::<Vec<_>>());
            let own = r.get(&filed.term().own)?.unwrap();
            assert_eq!(held(r, &filed.term(), &own)?, model.len() as u64);
            let mut walk = postings.walk();
            for place in 0..45 {
                assert_eq!(postings.at(place)?, model.get(&place).map(Vec::as_slice), "at {place}");
                assert_eq!(walk.to(place)?, model.get(&place).map(Vec::as_slice), "walking to {place}");
            }
            for (place, counts) in model.range(45..) {
                assert_eq!(postings.at(*place)?, Some(counts.as_slice()));
                assert_eq!(postings.at(place + 1)?, model.get(&(place + 1)).map(Vec::as_slice));
                assert_eq!(walk.to(*place)?, Some(counts.as_slice()));
            }
            Ok(())
        }).unwrap();
    }

    #[test]
    fn a_block_is_the_largest_range_holding_no_more_than_a_key_does(
        places in prop::collection::btree_set(place(), 2..90),
        capacity in 1usize..6,
    ) {
        let places: Vec<u64> = places.into_iter().collect();
        prop_assume!(places.len() > capacity);
        let list: Vec<(u64, [u8; 0])> = places.iter().map(|place| (*place, [])).collect();
        let found: Vec<(u64, u32, Vec<u64>)> = blocks(&list, capacity)
            .into_iter()
            .map(|(start, level, held)| (start, level, held.iter().map(|(place, _)| *place).collect()))
            .collect();
        let mut expected = Vec::new();
        by_halves(&places, 0, 63, capacity, &mut expected);
        prop_assert_eq!(found, expected);
    }
}

#[test]
fn a_term_moves_into_blocks_past_what_its_key_holds_and_back_when_few_hold_it() {
    let filed = Filed::new(1, 2);
    let one = counts(&[(0, vec![0])], 1);
    let file = |place: u64| filed.write(false, |t| put(t, &filed.term(), place, &one)).unwrap();
    let unfile = |place: u64| filed.write(false, |t| remove(t, &filed.term(), place)).unwrap();
    let own = || filed.stored()[0].1.clone();

    file(0);
    file(1);
    // Two entries, each a place past the last and a count of one
    assert_eq!(filed.stored(), vec![(filed.term().own, vec![2, 0, 1, 1, 1])]);
    // A third is more than the key holds: the halves of the first range that parts them
    file(2);
    assert_eq!(own(), IN_BLOCKS);
    assert_eq!(filed.blocks(), vec![(0, 1, vec![0, 1]), (2, 1, vec![2])]);
    // A place no block holds takes the largest range that leaves the others alone
    file(5);
    assert_eq!(filed.blocks(), vec![(0, 1, vec![0, 1]), (2, 1, vec![2]), (4, 2, vec![5])]);
    // One more in a full block splits it, and the half the entries left is no block
    file(4);
    file(7);
    assert_eq!(filed.blocks(), vec![(0, 1, vec![0, 1]), (2, 1, vec![2]), (4, 1, vec![4, 5]), (6, 1, vec![7])]);
    unfile(7);
    unfile(4);
    assert_eq!(filed.blocks(), vec![(0, 1, vec![0, 1]), (2, 1, vec![2]), (4, 2, vec![5])]);
    // A block emptied beside one that is then a block's worth: that one's range grows to the next block's
    unfile(2);
    assert_eq!(filed.blocks(), vec![(0, 2, vec![0, 1]), (4, 2, vec![5])]);
    // Counts written again in a block, and a place the term doesn't hold taken out, move nothing
    file(5);
    unfile(6);
    unfile(3);
    assert_eq!(filed.blocks(), vec![(0, 2, vec![0, 1]), (4, 2, vec![5])]);
    // Few enough for the term's own key again
    unfile(5);
    assert_eq!(filed.stored(), vec![(filed.term().own, vec![2, 0, 1, 1, 1])]);
    unfile(9);
    unfile(0);
    unfile(1);
    assert_eq!(filed.stored(), vec![]);
    unfile(1);
    assert_eq!(filed.stored(), vec![]);
}

#[test]
fn two_blocks_join_into_the_range_between_their_neighbours() {
    let filed = Filed::new(1, 2);
    let one = counts(&[(0, vec![0])], 1);
    for place in [16, 17, 20, 40, 41, 300] {
        filed.write(false, |t| put(t, &filed.term(), place, &one)).unwrap();
    }
    assert_eq!(
        filed.blocks(),
        vec![(16, 2, vec![16, 17]), (20, 2, vec![20]), (32, 5, vec![40, 41]), (256, 8, vec![300])]
    );
    // 17 gone, 16 and 20 are a block's worth: of the range up to the block at 32, which the block before it, were
    // there one, would also bound
    filed.write(false, |t| remove(t, &filed.term(), 17)).unwrap();
    assert_eq!(filed.blocks(), vec![(0, 5, vec![16, 20]), (32, 5, vec![40, 41]), (256, 8, vec![300])]);
    filed.write(false, |t| put(t, &filed.term(), 2, &one)).unwrap();
    assert_eq!(
        filed.blocks(),
        vec![(0, 4, vec![2]), (16, 4, vec![16, 20]), (32, 5, vec![40, 41]), (256, 8, vec![300])]
    );
    // A block emptied beside blocks smaller than its half of their range: more than a block's worth, and left alone
    filed.write(false, |t| remove(t, &filed.term(), 300)).unwrap();
    assert_eq!(filed.blocks(), vec![(0, 4, vec![2]), (16, 4, vec![16, 20]), (32, 5, vec![40, 41])]);
    filed.write(false, |t| remove(t, &filed.term(), 2)).unwrap();
    filed.write(false, |t| remove(t, &filed.term(), 41)).unwrap();
    assert_eq!(filed.blocks(), vec![(0, 5, vec![16, 20]), (32, 5, vec![40])]);

    // Two that join with a block before them and none after: the range up from that block's
    let filed = Filed::new(1, 2);
    for place in [1, 2, 600, 700, 800] {
        filed.write(false, |t| put(t, &filed.term(), place, &one)).unwrap();
    }
    assert_eq!(filed.blocks(), vec![(0, 9, vec![1, 2]), (512, 8, vec![600, 700]), (768, 8, vec![800])]);
    filed.write(false, |t| remove(t, &filed.term(), 700)).unwrap();
    assert_eq!(filed.blocks(), vec![(0, 9, vec![1, 2]), (512, 9, vec![600, 800])]);
}

#[test]
fn a_savepoint_rolled_back_leaves_a_terms_keys_as_they_were() {
    let filed = Filed::new(1, 2);
    let one = counts(&[(0, vec![0])], 1);
    for place in [0, 1, 2, 5] {
        filed.write(false, |t| put(t, &filed.term(), place, &one)).unwrap();
    }
    let before = filed.stored();
    // A split, a new block, a join back into the term's own key and a block emptied, each undone
    for places in [vec![(4, true), (7, true)], vec![(900, true)], vec![(5, false), (2, false)], vec![(2, false)]] {
        filed
            .write(true, |t| {
                for (place, filing) in &places {
                    match filing {
                        true => put(t, &filed.term(), *place, &one)?,
                        false => remove(t, &filed.term(), *place)?,
                    }
                }
                t.rollback_to(1)
            })
            .unwrap();
        assert_eq!(filed.stored(), before, "{places:?}");
    }
}

#[test]
fn counts_are_one_number_for_one_field_and_a_bit_a_field_then_a_number_each_for_several() {
    assert_eq!(counts(&[(0, vec![4, 9, 11])], 1), vec![3]);
    assert_eq!(counts(&[(0, (0..200).collect())], 1), vec![0xc8, 0x01]);
    // Of three fields, the first and the third
    assert_eq!(counts(&[(0, vec![1]), (2, vec![3, 4])], 3), vec![0b101, 1, 2]);
    // Of nine, the bits take two bytes
    assert_eq!(counts(&[(1, vec![1]), (8, vec![3, 4])], 9), vec![0b10, 0b1, 1, 2]);
    let read = |stored: &[u8], fields: usize| {
        let mut out = Vec::new();
        counted(stored, fields, |field, count| out.push((field, count))).map(|()| out)
    };
    assert_eq!(read(&[3], 1).unwrap(), vec![(0, 3)]);
    assert_eq!(read(&[0xc8, 0x01], 1).unwrap(), vec![(0, 200)]);
    assert_eq!(read(&[0b101, 1, 2], 3).unwrap(), vec![(0, 1), (2, 2)]);
    assert_eq!(read(&[0b10, 0b1, 1, 2], 9).unwrap(), vec![(1, 1), (8, 2)]);
    // Bits with no number after them, and no bits at all
    assert_eq!(read(&[0b101, 1], 3).unwrap_err().code, ErrorCode::Corrupted);
    assert_eq!(read(&[0b10], 9).unwrap_err().code, ErrorCode::Corrupted);
    assert_eq!(read(&[], 1).unwrap_err().code, ErrorCode::Corrupted);
}

#[test]
fn a_number_is_seven_bits_a_byte_lowest_first_and_ends_at_a_byte_under_128() {
    for (n, bytes) in [
        (0u64, vec![0]),
        (127, vec![0x7f]),
        (128, vec![0x80, 1]),
        (300, vec![0xac, 2]),
        (16_384, vec![0x80, 0x80, 1]),
        (u64::MAX, vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1]),
    ] {
        let mut out = Vec::new();
        put_varint(&mut out, n);
        assert_eq!(out, bytes, "{n}");
        let mut rest = &bytes[..];
        assert_eq!(varint(&mut rest).unwrap(), n);
        assert!(rest.is_empty());
    }
    // One that never ends, and one cut short
    assert_eq!(varint(&mut &[0x80; 11][..]).unwrap_err().code, ErrorCode::Corrupted);
    assert_eq!(varint(&mut &[0x80][..]).unwrap_err().code, ErrorCode::Corrupted);
}

#[test]
fn places_that_dont_read_are_refused_as_corrupt() {
    let raw = |own: &[u8], block: Option<(u64, &[u8])>| {
        let filed = Filed::new(1, 2);
        let term = filed.term();
        filed
            .write(false, |t| {
                t.put(&term.own, own)?;
                match block {
                    Some((start, stored)) => t.put(&term.block(start), stored),
                    None => Ok(()),
                }
            })
            .unwrap();
        filed
    };
    let reading = |filed: &Filed| filed.snapshot(|r| read(r, &filed.term()).map(|_| ())).unwrap_err().code;
    // An entry with no counts, a place past the last a u64 holds, and a block with no entries' count
    assert_eq!(reading(&raw(&[1, 5], None)), ErrorCode::Corrupted);
    let past = [2, 1, 1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1, 1];
    assert_eq!(reading(&raw(&past, None)), ErrorCode::Corrupted);
    assert_eq!(reading(&raw(&IN_BLOCKS, Some((0, &[])))), ErrorCode::Corrupted);
    // Places out of order: one twice, and one before the block ahead of it reached
    assert_eq!(reading(&raw(&IN_BLOCKS, Some((8, &[3, 2, 4, 1, 0, 1])))), ErrorCode::Corrupted);
    let filed = raw(&IN_BLOCKS, Some((0, &[3, 1, 0x80, 0x80, 0x80, 0x80, 0x20, 1])));
    filed.write(false, |t| t.put(&filed.term().block(8), &[3, 1, 1, 1])).unwrap();
    assert_eq!(reading(&filed), ErrorCode::Corrupted, "a place under 2^32 after one past it");
    // How many hold a term whose block doesn't say
    let filed = raw(&IN_BLOCKS, Some((0, &[3])));
    assert_eq!(
        filed.snapshot(|r| held(r, &filed.term(), &IN_BLOCKS)).unwrap_err().code,
        ErrorCode::Corrupted,
        "a block's count that doesn't read"
    );
    // A write that meets a term in blocks with no block has nothing to place the entity beside
    let filed = raw(&IN_BLOCKS, None);
    let one = counts(&[(0, vec![0])], 1);
    assert_eq!(filed.write(false, |t| put(t, &filed.term(), 3, &one)).unwrap_err().code, ErrorCode::Corrupted);
    // A write that meets a place twice in the term's own key lays out nothing from it
    let filed = raw(&[3, 5, 1, 0, 1, 0, 1], None);
    assert_eq!(filed.write(false, |t| put(t, &filed.term(), 7, &one)).unwrap_err().code, ErrorCode::Corrupted);
    assert_eq!(filed.write(false, |t| remove(t, &filed.term(), 5)).unwrap_err().code, ErrorCode::Corrupted);
    // A block that starts at no multiple of its range's size, and one whose every place is more than a part holds
    assert_eq!(reading(&raw(&IN_BLOCKS, Some((3, &[1, 1, 0, 1])))), ErrorCode::Corrupted);
    let filed = raw(&IN_BLOCKS, Some((2, &[1, 1, 0, 1])));
    assert!(filed.snapshot(|r| read(r, &filed.term())).is_ok(), "the same block at a multiple of its size");
    assert_eq!(reading(&raw(&IN_BLOCKS, Some((0, &[33, 0x80, 0x80, 0x80, 0x80, 0x20])))), ErrorCode::Corrupted);
    // Counts that add up to more than a u64 holds
    let most = [3, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1];
    let filed = raw(&IN_BLOCKS, Some((0, &most)));
    filed.write(false, |t| t.put(&filed.term().block(8), &most)).unwrap();
    assert_eq!(filed.snapshot(|r| held(r, &filed.term(), &IN_BLOCKS)).unwrap_err().code, ErrorCode::Corrupted);
    // A block whose range is no power of two a place can be in
    let filed = raw(&IN_BLOCKS, Some((0, &[70, 1, 0, 1])));
    assert_eq!(filed.write(false, |t| remove(t, &filed.term(), 0)).unwrap_err().code, ErrorCode::Corrupted);
    // A key that counts more entries than it holds, and one that counts fewer: read by a search, or by a write
    for own in [&[2, 5, 1][..], &[1, 5, 1, 1, 1]] {
        let filed = raw(own, None);
        assert_eq!(reading(&filed), ErrorCode::Corrupted, "{own:?}");
        assert_eq!(filed.write(false, |t| put(t, &filed.term(), 9, &one)).unwrap_err().code, ErrorCode::Corrupted);
        assert_eq!(filed.write(false, |t| remove(t, &filed.term(), 5)).unwrap_err().code, ErrorCode::Corrupted);
    }
    let filed = raw(&IN_BLOCKS, Some((4, &[2, 2, 0, 1])));
    assert_eq!(reading(&filed), ErrorCode::Corrupted, "a block of four places and one entry that counts two");
    assert_eq!(filed.write(false, |t| put(t, &filed.term(), 5, &one)).unwrap_err().code, ErrorCode::Corrupted);
    // A block of the places 4 and 5 with an entry at 6, which is the next block's: no write files beside it. The same
    // block with its entry at 5, its last place, reads and is written to
    let filed = raw(&IN_BLOCKS, Some((4, &[1, 1, 2, 1])));
    assert_eq!(reading(&filed), ErrorCode::Corrupted);
    assert_eq!(filed.write(false, |t| put(t, &filed.term(), 4, &one)).unwrap_err().code, ErrorCode::Corrupted);
    assert_eq!(filed.write(false, |t| remove(t, &filed.term(), 5)).unwrap_err().code, ErrorCode::Corrupted);
    let filed = raw(&IN_BLOCKS, Some((4, &[1, 1, 1, 1])));
    assert!(filed.snapshot(|r| read(r, &filed.term())).is_ok());
    filed.write(false, |t| put(t, &filed.term(), 4, &one)).unwrap();
    assert_eq!(filed.blocks(), vec![(4, 1, vec![4, 5])]);
    // A write that meets a block at no multiple of its range's size leaves it as it is
    let filed = raw(&IN_BLOCKS, Some((3, &[1, 1, 0, 1])));
    let before = filed.stored();
    assert_eq!(filed.write(false, |t| put(t, &filed.term(), 3, &one)).unwrap_err().code, ErrorCode::Corrupted);
    assert_eq!(filed.stored(), before);
}

#[test]
fn a_block_whose_every_place_is_held_is_read_as_its_range() {
    let filed = Filed::new(1, 4);
    let one = counts(&[(0, vec![0])], 1);
    // Two blocks of four places hold four each; the place after them is held by none, and the block past it by one
    let places: Vec<u64> = (0..8).chain([12]).collect();
    for place in &places {
        filed.write(false, |t| put(t, &filed.term(), *place, &one)).unwrap();
    }
    assert_eq!(filed.blocks(), vec![(0, 2, vec![0, 1, 2, 3]), (4, 2, vec![4, 5, 6, 7]), (8, 3, vec![12])]);
    filed
        .snapshot(|r| {
            let postings = read(r, &filed.term())?.unwrap();
            assert_eq!(postings.places.iter().collect::<Vec<_>>(), places);
            assert_eq!(held(r, &filed.term(), &IN_BLOCKS)?, 9);
            // A place of such a block still has its counts where a search asks for them
            assert_eq!(postings.at(5)?, Some(&one[..]));
            assert_eq!(postings.walk().to(6)?, Some(&one[..]));
            Ok(())
        })
        .unwrap();
    // Such a block's entries are first read where a search asks for one: those that don't read are refused there, not
    // taken for a place the term doesn't hold
    let filed = Filed::new(1, 4);
    filed
        .write(false, |t| {
            t.put(&filed.term().own, &IN_BLOCKS)?;
            t.put(&filed.term().block(0), &[1, 2, 0xff])
        })
        .unwrap();
    filed
        .snapshot(|r| {
            let postings = read(r, &filed.term())?.unwrap();
            assert_eq!(postings.places.iter().collect::<Vec<_>>(), [0, 1]);
            assert_eq!(postings.at(1).unwrap_err().code, ErrorCode::Corrupted);
            assert_eq!(postings.walk().to(0).unwrap_err().code, ErrorCode::Corrupted);
            let entries: Vec<_> = postings.entries().collect();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].as_ref().unwrap_err().code, ErrorCode::Corrupted);
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_frequency_is_each_fields_count_at_its_weight() {
    // One field: its count at its weight, whether a byte holds the count or two do
    assert_eq!(weighted(&[3], &[2.0]).unwrap(), 6.0);
    assert_eq!(weighted(&[0x7f], &[1.0]).unwrap(), 127.0);
    assert_eq!(weighted(&[0x80, 1], &[1.0]).unwrap(), 128.0);
    // Several: each field holding the term, and nothing for a field weighing nothing
    let held = counts(&[(0, vec![1, 2]), (2, vec![9])], 3);
    assert_eq!(weighted(&held, &[3.0, 100.0, 0.5]).unwrap(), 6.5);
    assert_eq!(weighted(&held, &[0.0, 1.0, 4.0]).unwrap(), 4.0);
    assert_eq!(weighted(&held, &[0.0, 1.0, 0.0]).unwrap(), 0.0);
    // A count that doesn't end is no count, of one field or of several
    for cut in [&[0x80][..], &[0xff], &[]] {
        assert_eq!(weighted(cut, &[1.0]).unwrap_err().code, ErrorCode::Corrupted, "{cut:?}");
    }
    assert_eq!(weighted(&[0b1, 0x80], &[1.0, 1.0]).unwrap_err().code, ErrorCode::Corrupted);
}
