//! The track index against a brute-force scan: random read sets added and removed, then random written keys, and the
//! owners the index finds for each are exactly those a scan of every read set finds. Keys are short strings over a few
//! byte values, so ranges meet, nest and share ends often; a range's end is exclusive, and an absent end runs to the
//! end of the store.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::{ReadSet, TrackIndex};

fn key() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop::sample::select(vec![0u8, b'a', b'b', 0xFF]), 0..=3)
}

fn read_set() -> impl Strategy<Value = ReadSet> {
    (prop::collection::btree_set(key(), 0..=4), prop::collection::vec((key(), prop::option::of(key())), 0..=4))
        .prop_map(|(keys, ranges)| ReadSet { keys, ranges })
}

#[derive(Debug, Clone)]
enum Step {
    Add(u8, ReadSet),
    Remove(u8),
    Write(Vec<Vec<u8>>),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0u8..8, read_set()).prop_map(|(owner, reads)| Step::Add(owner, reads)),
        (0u8..8).prop_map(Step::Remove),
        prop::collection::vec(key(), 1..=3).prop_map(Step::Write),
    ]
}

/// Who a brute-force scan of every owner's reads finds for a written key
fn scan(held: &BTreeMap<u8, Vec<ReadSet>>, written: &[u8]) -> BTreeSet<u8> {
    held.iter()
        .filter(|(_, sets)| {
            sets.iter().any(|reads| {
                reads.keys.contains(written)
                    || reads.ranges.iter().any(|(low, high)| {
                        low.as_slice() <= written && high.as_deref().is_none_or(|high| written < high)
                    })
            })
        })
        .map(|(owner, _)| *owner)
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn finds_exactly_the_owners_a_scan_of_every_read_set_finds(steps in prop::collection::vec(step(), 0..40)) {
        let mut index = TrackIndex::<u8>::default();
        let mut held: BTreeMap<u8, Vec<ReadSet>> = BTreeMap::new();
        for step in steps {
            match step {
                Step::Add(owner, reads) => {
                    index.add(&owner, reads.clone());
                    held.entry(owner).or_default().push(reads);
                }
                Step::Remove(owner) => {
                    index.remove(&owner);
                    held.remove(&owner);
                }
                Step::Write(keys) => {
                    let mut expected = BTreeSet::new();
                    for key in &keys {
                        expected.extend(scan(&held, key));
                    }
                    prop_assert_eq!(index.touched_by(keys.iter().map(Vec::as_slice)), expected);
                }
            }
        }
        prop_assert_eq!(index.owners(), held.keys().copied().collect::<BTreeSet<_>>());
        for owner in held.keys() {
            index.remove(owner);
        }
        prop_assert_eq!(index.size(), (0, 0));
    }
}

#[test]
fn finds_a_range_by_a_key_at_its_low_end_and_not_at_its_high_end() {
    let mut index = TrackIndex::<&str>::default();
    let range = |low: &[u8], high: Option<&[u8]>| ReadSet {
        keys: BTreeSet::new(),
        ranges: vec![(low.to_vec(), high.map(<[u8]>::to_vec))],
    };
    index.add(&"r", range(b"b", Some(b"d")));
    index.add(&"open", range(b"c", None));
    let touched = |key: &[u8]| index.touched_by([key]).into_iter().collect::<Vec<_>>();
    assert_eq!(touched(b"b"), ["r"]);
    assert_eq!(touched(b"d"), ["open"]);
    assert_eq!(touched(&[0xFF, 0xFF]), ["open"]);
    assert!(touched(b"a").is_empty());
}

#[test]
fn an_owner_reading_the_same_keys_and_ranges_again_files_them_once() {
    let mut index = TrackIndex::<&str>::default();
    let reads = || ReadSet {
        keys: BTreeSet::from([b"k".to_vec()]),
        ranges: vec![(b"b".to_vec(), Some(b"d".to_vec())), (b"x".to_vec(), None)],
    };
    for _ in 0..3 {
        index.add(&"w", reads());
    }
    assert_eq!(index.size(), (1, 2));
    index.add(&"other", reads());
    assert_eq!(index.size(), (1, 4), "another owner's reads are its own");
    assert_eq!(index.touched_by([b"c".as_slice()]).into_iter().collect::<Vec<_>>(), ["other", "w"]);
    index.remove(&"w");
    assert_eq!(index.size(), (1, 2));
    index.add(&"w", reads());
    assert_eq!(index.size(), (1, 4), "a removed owner files its reads anew");
}
