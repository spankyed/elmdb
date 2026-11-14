//! A string's place in the order a value index keeps (`docs/roadmap/string-order.md`): its sort key, the bytes the
//! schema's collator orders it by, as far as the key it is filed under has room for them.
//!
//! Sort keys compare by their bytes as the collator compares their strings, and have no bound a key could hold, so
//! one is cut as it is made: the collator hands its bytes to a sink that keeps the first `room` of them and refuses
//! the next, which stops it there. A cut key is exactly `room` bytes, the first of the whole key, and a whole one is
//! no longer; so of two strings filed under `(sort key, cut)`, the lesser key is the lesser string, but for two whose
//! keys were cut and are the same bytes, which differ past the cut if they differ.

use icu_collator::{CollationKeySink, CollatorBorrowed};

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

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use proptest::prelude::*;

    use super::*;
    use crate::engine::collator;

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
