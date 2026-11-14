//! A string's place in the order a value index keeps (`docs/roadmap/string-order.md`): its sort key, the bytes the
//! schema's collator orders it by, as far as the key it is filed under has room for them.
//!
//! Sort keys compare by their bytes as the collator compares their strings, and have no bound a key could hold, so
//! one is cut as it is made: the collator hands its bytes to a sink that keeps the first `room` of them and refuses
//! the next, which stops it there. A cut key is exactly `room` bytes, the first of the whole key, and a whole one is
//! no longer; so of two strings filed under `(sort key, cut)`, the lesser key is the lesser string, but for two whose
//! keys were cut and are the same bytes, which differ past the cut if they differ.

use icu_collator::{CollationKeySink, CollatorBorrowed};
use sha2::{Digest, Sha256};

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

    fn write(&mut self, (): &mut (), bytes: &[u8]) -> Result<(), Full> {
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

    fn finish(&mut self, (): ()) -> Result<(), Full> {
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

/// The stamp of the keys this elmdb files for a schema's `collation`
pub fn stamp(collation: &str) -> crate::error::Result<Stamp> {
    let collator = super::collator(collation)?;
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
        part(&sort_key(&collator, probe, usize::MAX).bytes);
    }
    Ok(hash.finalize().into())
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
                // Under these names and values a sort key of the least room wouldn't fit before a string
                let begins = keys::order_range(&entity_type, &index, build, &leading).len();
                prop_assert!(begins + 2 + keys::LEAST_ORDER_ROOM + 1 + 258 > most);
                return Ok(());
            };
            prop_assert!(room >= keys::LEAST_ORDER_ROOM);
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
