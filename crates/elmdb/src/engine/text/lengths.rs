//! Each field's length in a text index's entities, by place (`docs/architecture/search.md`, Text search as built):
//! `d/fl type index block`, a block of `PLACES` places, which a search reads a match's lengths from without reading
//! the entity's document.
//!
//! A block holds a width, then its places' lengths: each field's, in field order, little-endian in `width` bytes. The
//! width is 1, 2 or 4, the least the block's longest length fits in. A place the index holds nothing of holds zeros,
//! and a block of nothing but zeros isn't stored. The lengths are in one of two forms, whichever is fewer bytes, the
//! first when they are as many:
//!
//! - every place's, from the block's first place to the last that holds any
//! - a bit a place for those that hold any, lowest place first, then those places' alone; the width's byte has
//!   `SPARSE` set
//!
//! Places are the store's, which every type's entities and every relation take in turn, so a type's entities may be
//! few of a block's places: the second form is what keeps a lone entity's lengths to its own bytes and the bits. Which
//! form, and so a block's bytes, follows from the lengths alone.

use crate::error::{Error, ErrorCode, Result};

/// How many places a block covers: at a byte a length, half a kilobyte a field
pub const PLACES: u64 = 512;

fn corrupt() -> Error {
    Error::new(ErrorCode::Corrupted, "A text index's lengths don't read: rebuild() derives them again")
}

/// The block a place's lengths are in
pub fn block_of(place: u64) -> u64 {
    place / PLACES
}

/// Where in its block a place's lengths are, among the places'
fn slot_of(place: u64) -> usize {
    (place % PLACES) as usize
}

/// A block as stored, for reading the lengths at its places
pub struct Lengths<B> {
    width: usize,
    /// Whether only the places holding a length have theirs here, after a bit a place saying which do
    sparse: bool,
    stored: B,
    fields: usize,
}

/// Set in a block's first byte, beside the width, when its lengths are those of the places its bits name
const SPARSE: u8 = 8;

/// How many bytes hold a bit for each place of a block
const BITS: usize = (PLACES / 8) as usize;

impl<B: AsRef<[u8]>> Lengths<B> {
    /// A block of an index of `fields` fields
    pub fn read(stored: B, fields: usize) -> Result<Self> {
        let (&first, rest) = stored.as_ref().split_first().ok_or_else(corrupt)?;
        let (sparse, width) = (first & SPARSE != 0, usize::from(first & !SPARSE));
        if !matches!(width, 1 | 2 | 4) || fields == 0 {
            return Err(corrupt());
        }
        let whole = if sparse {
            // As many places' lengths as bits are set
            let (bits, lengths) = rest.split_at_checked(BITS).ok_or_else(corrupt)?;
            let held: usize = bits.iter().map(|byte| byte.count_ones() as usize).sum();
            lengths.len() == held * width * fields
        } else {
            rest.len() % (width * fields) == 0
        };
        if !whole {
            return Err(corrupt());
        }
        Ok(Lengths { width, sparse, stored, fields })
    }

    /// The bits saying which places hold a length, of a block that keeps them, and the lengths
    fn parts(&self) -> (&[u8], &[u8]) {
        self.stored.as_ref()[1..].split_at(if self.sparse { BITS } else { 0 })
    }

    /// A field's length at a place of the block: 0 where the block holds nothing
    #[inline]
    pub fn at(&self, place: u64, field: usize) -> u32 {
        let (bits, lengths) = self.parts();
        let slot = slot_of(place);
        let at = if self.sparse {
            let (byte, bit) = (slot / 8, 1u8 << (slot % 8));
            if bits[byte] & bit == 0 {
                return 0;
            }
            // Where its lengths are among those kept: after those of the places before it that hold any
            let before: u32 = bits[..byte].iter().map(|byte| byte.count_ones()).sum();
            (before + (bits[byte] & (bit - 1)).count_ones()) as usize
        } else {
            slot
        };
        let from = (at * self.fields + field) * self.width;
        lengths.get(from..from + self.width).map_or(0, length)
    }

    /// Every place's lengths from the block's first place to the last that holds any, each place's in field order
    fn all(&self) -> Vec<u32> {
        let (bits, lengths) = self.parts();
        let mut kept = lengths.chunks(self.width).map(length);
        if !self.sparse {
            return kept.collect();
        }
        let held = |slot: usize| bits[slot / 8] & (1 << (slot % 8)) != 0;
        let places = (0..PLACES as usize).rposition(held).map_or(0, |last| last + 1);
        let mut all = vec![0; places * self.fields];
        for slot in (0..places).filter(|slot| held(*slot)) {
            for length in &mut all[slot * self.fields..][..self.fields] {
                *length = kept.next().unwrap_or(0);
            }
        }
        all
    }
}

/// A length as a block holds it, in the one, two or four bytes of the block's width
#[inline]
fn length(held: &[u8]) -> u32 {
    match *held {
        [length] => u32::from(length),
        [low, high] => u32::from(u16::from_le_bytes([low, high])),
        [a, b, c, d] => u32::from_le_bytes([a, b, c, d]),
        _ => 0,
    }
}

/// What a block of these lengths holds, place by place and each place's in field order, or nothing for one of zeros
fn stored(lengths: &[u32], fields: usize) -> Option<Vec<u8>> {
    let holds = |place: &[u32]| place.iter().any(|length| *length > 0);
    let places = lengths.chunks(fields).rposition(holds)? + 1;
    let lengths = &lengths[..places * fields];
    let longest = lengths.iter().copied().max().unwrap_or(0);
    let width = if longest <= 0xff {
        1
    } else if longest <= 0xffff {
        2
    } else {
        4
    };
    // Only the places holding a length, with a bit a place, when that is fewer bytes than every place's
    let held = lengths.chunks(fields).filter(|place| holds(place)).count();
    let sparse = BITS + held * fields * width < places * fields * width;
    let mut out = vec![width as u8 | if sparse { SPARSE } else { 0 }];
    if sparse {
        let mut bits = [0u8; BITS];
        for (slot, _) in lengths.chunks(fields).enumerate().filter(|(_, place)| holds(place)) {
            bits[slot / 8] |= 1 << (slot % 8);
        }
        out.extend_from_slice(&bits);
    }
    for place in lengths.chunks(fields).filter(|place| !sparse || holds(place)) {
        for length in place {
            out.extend_from_slice(&length.to_le_bytes()[..width]);
        }
    }
    Some(out)
}

/// The block with `lengths` at `place`, zeros for an entity the index holds nothing of: what the place held before,
/// and what the block holds now, nothing when that is no length at all
pub fn set(before: Option<&[u8]>, place: u64, lengths: &[u32]) -> Result<(Vec<u32>, Option<Vec<u8>>)> {
    let fields = lengths.len();
    let mut all = match before {
        Some(before) => Lengths::read(before, fields)?.all(),
        None => Vec::new(),
    };
    let from = slot_of(place) * fields;
    all.resize(all.len().max(from + fields), 0);
    let held = all[from..from + fields].to_vec();
    all[from..from + fields].copy_from_slice(lengths);
    Ok((held, stored(&all, fields)))
}

/// What a block holds, from the lengths at each of its places that holds any: `(place, lengths)`, in any order
pub fn block(held: &[(u64, Vec<u32>)], fields: usize) -> Option<Vec<u8>> {
    let places = held.iter().map(|(place, _)| slot_of(*place) + 1).max()?;
    let mut all = vec![0; places * fields];
    for (place, lengths) in held {
        let from = slot_of(*place) * fields;
        all[from..from + fields].copy_from_slice(lengths);
    }
    stored(&all, fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_is_as_wide_as_its_longest_length_and_as_long_as_its_last_place_holding_one() {
        // Two fields; the place after the block's first holds 3 and 0
        let (before, block) = set(None, PLACES + 1, &[3, 0]).unwrap();
        assert_eq!(before, vec![0, 0]);
        let block = block.unwrap();
        assert_eq!(block, vec![1, 0, 0, 3, 0]);
        // A length past a byte widens every one to two, and past two bytes to four
        let (before, wide) = set(Some(&block), PLACES, &[0, 256]).unwrap();
        assert_eq!(before, vec![0, 0]);
        let wide = wide.unwrap();
        assert_eq!(wide, vec![2, 0, 0, 0, 1, 3, 0, 0, 0]);
        let (_, wider) = set(Some(&wide), PLACES + 2, &[65_536, 255]).unwrap();
        let wider = wider.unwrap();
        assert_eq!(wider[0], 4);
        assert_eq!(wider.len(), 1 + 3 * 2 * 4);
        let read = Lengths::read(&wider, 2).unwrap();
        let at = |place: u64| [read.at(place, 0), read.at(place, 1)];
        assert_eq!([at(PLACES), at(PLACES + 1), at(PLACES + 2)], [[0, 256], [3, 0], [65_536, 255]]);
        // Past its last place, and in any other block's reading of the same slots, nothing
        assert_eq!(at(PLACES + 3), [0, 0]);
        assert_eq!(at(PLACES - 1), [0, 0]);
        // The longest gone, the block narrows again; its last place emptied, it ends at the one before
        let (before, narrow) = set(Some(&wider), PLACES + 2, &[0, 0]).unwrap();
        assert_eq!(before, vec![65_536, 255]);
        assert_eq!(narrow.unwrap(), wide);
        // 255 and 65,535 are the most one byte and two hold
        assert_eq!(set(None, 0, &[255]).unwrap().1.unwrap(), vec![1, 255]);
        assert_eq!(set(None, 0, &[65_535]).unwrap().1.unwrap(), vec![2, 255, 255]);
        // Its only place emptied, there is no block; emptying a place it never held changes nothing
        assert_eq!(set(Some(&[1, 7]), 0, &[0]).unwrap(), (vec![7], None));
        assert_eq!(set(Some(&[1, 7]), 5, &[0]).unwrap(), (vec![0], Some(vec![1, 7])));
        assert_eq!(set(None, 5, &[0]).unwrap(), (vec![0], None));
    }

    #[test]
    fn a_block_built_from_its_places_is_the_block_setting_each_makes() {
        let held = vec![(PLACES * 3 + 4, vec![1, 300]), (PLACES * 3, vec![9, 0]), (PLACES * 3 + 2, vec![0, 2])];
        let mut one_by_one = None;
        for (place, lengths) in &held {
            one_by_one = set(one_by_one.as_deref(), *place, lengths).unwrap().1;
        }
        assert_eq!(block(&held, 2), one_by_one);
        assert_eq!(block(&held, 2).unwrap().len(), 1 + 5 * 2 * 2);
        // Places holding only zeros, and no place at all, are no block
        assert_eq!(block(&[(7, vec![0, 0])], 2), None);
        assert_eq!(block(&[], 2), None);
        assert_eq!(block_of(PLACES * 3 + 4), 3);
        assert_eq!(block_of(PLACES - 1), 0);
    }

    #[test]
    fn a_block_of_few_places_far_apart_keeps_their_lengths_alone_after_a_bit_a_place() {
        // The block's last place alone: a byte for the width, the bits, and its one length, where every place's
        // lengths would be 512 bytes
        let (_, lone) = set(None, PLACES - 1, &[7]).unwrap();
        let lone = lone.unwrap();
        let mut expected = vec![1 | SPARSE];
        expected.extend([0; BITS - 1]);
        expected.extend([0x80, 7]);
        assert_eq!(lone, expected);
        let read = Lengths::read(&lone, 1).unwrap();
        assert_eq!([read.at(PLACES - 1, 0), read.at(PLACES - 2, 0), read.at(0, 0)], [7, 0, 0]);
        // Two fields two bytes wide, at places with others between: each place's lengths where the places before it
        // that hold any leave them
        let held = vec![(9, vec![1, 300]), (200, vec![0, 2]), (201, vec![65_535, 0]), (400, vec![4, 4])];
        let stored = block(&held, 2).unwrap();
        assert_eq!(stored.len(), 1 + BITS + 4 * 2 * 2);
        assert_eq!(stored[0], 2 | SPARSE);
        let read = Lengths::read(&stored, 2).unwrap();
        for (place, lengths) in &held {
            assert_eq!([read.at(*place, 0), read.at(*place, 1)], lengths[..], "{place}");
        }
        assert_eq!([read.at(8, 0), read.at(10, 1), read.at(199, 1), read.at(202, 0), read.at(401, 0)], [0; 5]);
        // A place set in it is read back from it, and one emptied leaves the others'
        let (before, next) = set(Some(&stored), 200, &[0, 0]).unwrap();
        assert_eq!(before, vec![0, 2]);
        assert_eq!(next, block(&[held[0].clone(), held[2].clone(), held[3].clone()], 2));
        // As many bytes either way is every place's: one length at the 65th place is 65 bytes with the bits or
        // without, and at the 66th one fewer with them
        assert_eq!(set(None, 64, &[5]).unwrap().1.unwrap()[0], 1);
        assert_eq!(set(None, 65, &[5]).unwrap().1.unwrap()[0], 1 | SPARSE);
        // Bits for more places than lengths follow, for fewer, and a block too short for its bits
        let mut short = lone.clone();
        short.pop();
        let mut long = lone.clone();
        long.push(1);
        for stored in [&short[..], &long[..], &[1 | SPARSE, 0, 0][..]] {
            assert_eq!(Lengths::read(stored, 1).err().map(|e| e.code), Some(ErrorCode::Corrupted));
        }
    }

    proptest::proptest! {
        #[test]
        fn a_blocks_bytes_follow_from_its_lengths_however_they_were_set_and_each_reads_back(
            fields in 1usize..4,
            ops in proptest::collection::vec(
                (
                    proptest::prop_oneof![0..PLACES, 0..6u64, 500..PLACES],
                    proptest::collection::vec(proptest::prop_oneof![
                        3 => proptest::strategy::Just(0u32), 3 => 1..200u32, 1 => 256..400u32, 1 => 65_536..70_000u32
                    ], 3),
                ),
                0..40,
            ),
        ) {
            let mut model: std::collections::BTreeMap<u64, Vec<u32>> = Default::default();
            let mut stored: Option<Vec<u8>> = None;
            for (place, lengths) in ops {
                let lengths = &lengths[..fields];
                let (before, next) = set(stored.as_deref(), place, lengths).unwrap();
                proptest::prop_assert_eq!(before, model.get(&place).cloned().unwrap_or(vec![0; fields]));
                model.insert(place, lengths.to_vec());
                let held: Vec<(u64, Vec<u32>)> = model.iter().map(|(place, lengths)| (*place, lengths.clone())).collect();
                proptest::prop_assert_eq!(&next, &block(&held, fields));
                stored = next;
                // No longer than every place's lengths, nor than the bits and the lengths of those holding any
                let holding: Vec<&Vec<u32>> = model.values().filter(|l| l.iter().any(|length| *length > 0)).collect();
                let Some(stored) = &stored else {
                    proptest::prop_assert!(holding.is_empty());
                    continue;
                };
                let width = usize::from(stored[0] & !SPARSE);
                let last = model.iter().filter(|(_, l)| l.iter().any(|length| *length > 0)).map(|(place, _)| *place).max().unwrap();
                let every = (last as usize + 1) * fields * width;
                let few = BITS + holding.len() * fields * width;
                proptest::prop_assert_eq!(stored.len(), 1 + every.min(few));
                let read = Lengths::read(stored, fields).unwrap();
                for place in 0..PLACES {
                    for field in 0..fields {
                        let expected = model.get(&place).map_or(0, |lengths| lengths[field]);
                        proptest::prop_assert_eq!(read.at(place, field), expected, "place {} field {}", place, field);
                    }
                }
            }
        }
    }

    #[test]
    fn a_block_of_another_width_or_of_part_of_a_place_is_refused_as_corrupt() {
        for stored in [&[][..], &[3, 0, 0, 0, 0, 0, 0], &[1, 0, 0, 0], &[2, 0, 0], &[0]] {
            assert_eq!(Lengths::read(stored, 2).err().map(|e| e.code), Some(ErrorCode::Corrupted), "{stored:?}");
        }
        assert_eq!(set(Some(&[5, 1]), 0, &[1]).unwrap_err().code, ErrorCode::Corrupted);
        assert!(Lengths::read(&[1], 2).is_ok());
        assert!(Lengths::read(&[4, 0, 0, 0, 0, 1, 0, 0, 0], 2).is_ok());
        assert_eq!(Lengths::read(&[1, 9], 0).err().map(|e| e.code), Some(ErrorCode::Corrupted));
    }
}
