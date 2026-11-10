//! Each field's length in a text index's entities, by place (`docs/architecture/search.md`, Text search as built):
//! `d/fl type index block`, a block of `PLACES` places, which a search reads a match's lengths from without reading
//! the entity's document.
//!
//! A block holds a width, then every place's lengths from the block's first place to the last that holds any: each
//! field's, in field order, little-endian in `width` bytes. The width is 1, 2 or 4, the least the block's longest
//! length fits in. A place the index holds nothing of holds zeros, and a block of nothing but zeros isn't stored, so a
//! block's bytes follow from the lengths alone.

use crate::error::{Error, ErrorCode, Result};

/// How many places a block covers: at a byte a length, half a kilobyte a field
pub const PLACES: u64 = 512;

fn corrupt() -> Error {
    Error::new(ErrorCode::Corrupted, "A text index's lengths don't read")
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
    stored: B,
    fields: usize,
}

impl<B: AsRef<[u8]>> Lengths<B> {
    /// A block of an index of `fields` fields
    pub fn read(stored: B, fields: usize) -> Result<Self> {
        let (&width, lengths) = stored.as_ref().split_first().ok_or_else(corrupt)?;
        let width = usize::from(width);
        if !matches!(width, 1 | 2 | 4) || fields == 0 || lengths.len() % (width * fields) != 0 {
            return Err(corrupt());
        }
        Ok(Lengths { width, stored, fields })
    }

    /// The lengths, after the width that starts the block
    fn lengths(&self) -> &[u8] {
        &self.stored.as_ref()[1..]
    }

    /// A field's length at a place of the block: 0 where the block holds nothing
    #[inline]
    pub fn at(&self, place: u64, field: usize) -> u32 {
        let from = (slot_of(place) * self.fields + field) * self.width;
        match self.lengths().get(from..from + self.width) {
            Some(&[length]) => u32::from(length),
            Some(&[low, high]) => u32::from(u16::from_le_bytes([low, high])),
            Some(&[a, b, c, d]) => u32::from_le_bytes([a, b, c, d]),
            _ => 0,
        }
    }

    /// Every length the block holds, place by place, each place's in field order
    fn all(&self) -> Vec<u32> {
        let places = self.lengths().len() / (self.width * self.fields);
        (0..places as u64).flat_map(|place| (0..self.fields).map(move |field| self.at(place, field))).collect()
    }
}

/// What a block of these lengths holds, place by place and each place's in field order, or nothing for one of zeros
fn stored(lengths: &[u32], fields: usize) -> Option<Vec<u8>> {
    let places = lengths.chunks(fields).rposition(|place| place.iter().any(|length| *length > 0))? + 1;
    let lengths = &lengths[..places * fields];
    let longest = lengths.iter().copied().max().unwrap_or(0);
    let width = if longest <= 0xff {
        1
    } else if longest <= 0xffff {
        2
    } else {
        4
    };
    let mut out = vec![width as u8];
    for length in lengths {
        out.extend_from_slice(&length.to_le_bytes()[..width]);
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
