//! Rarest-First Trigram projection (1 sub-DB per index, 1-byte table tags).
//!
//! Layout inside the index's pooled `SubDbSlot`:
//! - `0x01 || trigram_utf8 || 0x00 || block(u64 BE)` -> serialized `RoaringBitmap` of `u16` offsets
//!   where `block = entity_key.0 >> 16` and `offset = (entity_key.0 & 0xFFFF) as u32`
//! - `0x02 || trigram_utf8` -> `u32 LE` exact document frequency (`trig_df`, O(1) rarest-first planning!)
//! - `0x03 || EntityKey(8B BE)` -> sorted compact list of the entity's indexed trigrams

use std::collections::BTreeSet;

use elmdb_engine::{EntityKey, FieldId, PackedRow};
use elmdb_storage::{KvCursor, KvRead, SubDbHandle};
use elmdb_wire::{CanonicalValue, ElmdbError, ErrorCode, Result};
use roaring::RoaringBitmap;

use crate::state::KvMut;

const TAG_TRIGRAM: u8 = 0x01;
const TAG_TRIG_DF: u8 = 0x02;
const TAG_DOC_TRIGS: u8 = 0x03;
const TAG_OVERFLOW: u8 = 0x04;

/// Maximum trigrams indexed per single entity to bound worst-case write amplification on huge blobs.
pub const MAX_TRIGRAMS_PER_DOC: usize = 16_384;

/// Extracts sorted, deduplicated lowercase 3-character Unicode trigrams from `text` using
/// full-string `str::to_lowercase()` so contextual case folding (e.g., Greek final sigma `Σ` -> `ς`)
/// matches `Contains` substring evaluation identically.
pub fn extract_trigrams(text: &str) -> Vec<String> {
    extract_trigrams_with_overflow(text).0
}

fn extract_trigrams_with_overflow(text: &str) -> (Vec<String>, bool) {
    let lower = text.to_lowercase();
    let mut set = BTreeSet::new();
    let mut iter = lower.chars();
    let (Some(mut c0), Some(mut c1)) = (iter.next(), iter.next()) else {
        return (Vec::new(), false);
    };
    let mut overflowed = false;
    for c2 in iter {
        if c0 != '\0' && c1 != '\0' && c2 != '\0' {
            let mut s = String::with_capacity(c0.len_utf8() + c1.len_utf8() + c2.len_utf8());
            s.push(c0);
            s.push(c1);
            s.push(c2);
            if set.len() >= MAX_TRIGRAMS_PER_DOC && !set.contains(&s) {
                overflowed = true;
                break;
            }
            set.insert(s);
        }
        c0 = c1;
        c1 = c2;
    }
    (set.into_iter().collect(), overflowed)
}

fn extract_row_trigrams(row: &PackedRow, fields: &[FieldId]) -> (Vec<String>, bool) {
    let mut set = BTreeSet::new();
    let mut overflowed = false;
    for &fid in fields {
        let Some(val) = row.fields.get(&fid) else { continue };
        match val {
            CanonicalValue::String(s) => {
                let (trigs, ovf) = extract_trigrams_with_overflow(s);
                if ovf {
                    overflowed = true;
                }
                for t in trigs {
                    if set.len() >= MAX_TRIGRAMS_PER_DOC && !set.contains(&t) {
                        return (set.into_iter().collect(), true);
                    }
                    set.insert(t);
                }
            }
            CanonicalValue::Array(items) => {
                for item in items {
                    if let CanonicalValue::String(s) = item {
                        let (trigs, ovf) = extract_trigrams_with_overflow(s);
                        if ovf {
                            overflowed = true;
                        }
                        for t in trigs {
                            if set.len() >= MAX_TRIGRAMS_PER_DOC && !set.contains(&t) {
                                return (set.into_iter().collect(), true);
                            }
                            set.insert(t);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (set.into_iter().collect(), overflowed)
}

fn overflow_key(key: EntityKey) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = TAG_OVERFLOW;
    k[1..9].copy_from_slice(&key.to_be_bytes());
    k
}

fn trigram_key(trig: &str, block: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + trig.len() + 1 + 8);
    k.push(TAG_TRIGRAM);
    k.extend_from_slice(trig.as_bytes());
    k.push(0x00);
    k.extend_from_slice(&block.to_be_bytes());
    k
}

fn df_key(trig: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + trig.len());
    k.push(TAG_TRIG_DF);
    k.extend_from_slice(trig.as_bytes());
    k
}

fn doc_trigs_key(key: EntityKey) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = TAG_DOC_TRIGS;
    k[1..9].copy_from_slice(&key.to_be_bytes());
    k
}

fn encode_doc_trigs(trigs: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + trigs.len() * 4);
    out.extend_from_slice(&(trigs.len() as u32).to_le_bytes());
    for t in trigs {
        out.push(t.len() as u8);
        out.extend_from_slice(t.as_bytes());
    }
    out
}

fn decode_doc_trigs(raw: &[u8]) -> Result<Vec<String>> {
    if raw.len() < 4 {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated doc trigrams"));
    }
    let count = u32::from_le_bytes(raw[0..4].try_into().expect("4")) as usize;
    let mut pos = 4usize;
    let mut out = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        if pos >= raw.len() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated doc trigram length"));
        }
        let len = raw[pos] as usize;
        pos += 1;
        if pos + len > raw.len() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated doc trigram bytes"));
        }
        let s = std::str::from_utf8(&raw[pos..pos + len])
            .map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "non-utf8 doc trigram"))?
            .to_owned();
        pos += len;
        out.push(s);
    }
    Ok(out)
}

fn encode_bitmap(bm: &RoaringBitmap) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(bm.serialized_size());
    bm.serialize_into(&mut buf).map_err(|e| ElmdbError::new(ErrorCode::Internal, e.to_string()))?;
    Ok(buf)
}

fn decode_bitmap(raw: &[u8]) -> Result<RoaringBitmap> {
    let bm = RoaringBitmap::deserialize_from(raw).map_err(|e| ElmdbError::new(ErrorCode::Corrupted, e.to_string()))?;
    if bm.max().is_some_and(|m| m > 0xFFFF) {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "trigram bitmap offset exceeds u16 block range"));
    }
    Ok(bm)
}

fn add_trigram<W: KvMut>(txn: &mut W, db: SubDbHandle, trig: &str, key: EntityKey) -> Result<()> {
    let block = key.0 >> 16;
    let offset = (key.0 & 0xFFFF) as u32;
    let tk = trigram_key(trig, block);
    let mut bm = match txn.get(db, &tk)? {
        Some(raw) => decode_bitmap(raw)?,
        None => RoaringBitmap::new(),
    };
    if bm.insert(offset) {
        txn.put(db, &tk, &encode_bitmap(&bm)?)?;
        let dk = df_key(trig);
        let cur_df = match txn.get(db, &dk)? {
            Some(raw) if raw.len() == 4 => u32::from_le_bytes(raw.try_into().expect("4")),
            Some(_) => return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt trig_df")),
            None => 0,
        };
        txn.put(db, &dk, &cur_df.saturating_add(1).to_le_bytes())?;
    }
    Ok(())
}

fn remove_trigram<W: KvMut>(txn: &mut W, db: SubDbHandle, trig: &str, key: EntityKey) -> Result<()> {
    let block = key.0 >> 16;
    let offset = (key.0 & 0xFFFF) as u32;
    let tk = trigram_key(trig, block);
    let Some(raw) = txn.get(db, &tk)? else {
        return Ok(());
    };
    let mut bm = decode_bitmap(raw)?;
    if bm.remove(offset) {
        if bm.is_empty() {
            txn.del(db, &tk)?;
        } else {
            txn.put(db, &tk, &encode_bitmap(&bm)?)?;
        }
        let dk = df_key(trig);
        if let Some(raw_df) = txn.get(db, &dk)? {
            if raw_df.len() != 4 {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt trig_df"));
            }
            let cur_df = u32::from_le_bytes(raw_df.try_into().expect("4"));
            let next_df = cur_df.saturating_sub(1);
            if next_df == 0 {
                txn.del(db, &dk)?;
            } else {
                txn.put(db, &dk, &next_df.to_le_bytes())?;
            }
        }
    }
    Ok(())
}

/// Applies an incremental live-row `(before, after)` delta to a Trigram index (diffing old vs new trigrams!).
pub fn apply_delta<W: KvMut>(
    txn: &mut W,
    db: SubDbHandle,
    fields: &[FieldId],
    key: EntityKey,
    _before: Option<&PackedRow>,
    after: Option<&PackedRow>,
) -> Result<()> {
    let dtk = doc_trigs_key(key);
    let ovk = overflow_key(key);
    let old_trigs = match txn.get(db, &dtk)?.map(ToOwned::to_owned) {
        Some(raw) => decode_doc_trigs(&raw)?,
        None => Vec::new(),
    };
    let old_overflowed = txn.get(db, &ovk)?.is_some();
    let (new_trigs, new_overflowed) = match after {
        Some(r) => extract_row_trigrams(r, fields),
        None => (Vec::new(), false),
    };
    if old_overflowed != new_overflowed {
        if new_overflowed {
            txn.put(db, &ovk, &[])?;
        } else {
            txn.del(db, &ovk)?;
        }
    }
    if old_trigs == new_trigs {
        return Ok(());
    }

    // Two-pointer sorted diff so unchanged trigrams are untouched!
    let mut i = 0;
    let mut j = 0;
    while i < old_trigs.len() && j < new_trigs.len() {
        match old_trigs[i].cmp(&new_trigs[j]) {
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => {
                remove_trigram(txn, db, &old_trigs[i], key)?;
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                add_trigram(txn, db, &new_trigs[j], key)?;
                j += 1;
            }
        }
    }
    while i < old_trigs.len() {
        remove_trigram(txn, db, &old_trigs[i], key)?;
        i += 1;
    }
    while j < new_trigs.len() {
        add_trigram(txn, db, &new_trigs[j], key)?;
        j += 1;
    }

    if new_trigs.is_empty() {
        txn.del(db, &dtk)?;
    } else {
        txn.put(db, &dtk, &encode_doc_trigs(&new_trigs))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rarest-First Trigram Intersection (consumed by `elmdb-query`)
// ---------------------------------------------------------------------------

/// O(1) document-frequency lookup for a trigram.
pub fn trigram_df<R: KvRead>(txn: &R, db: SubDbHandle, trig: &str) -> Result<u32> {
    match txn.get(db, &df_key(trig))? {
        Some(raw) if raw.len() == 4 => Ok(u32::from_le_bytes(raw.try_into().expect("4"))),
        Some(_) => Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt trig_df")),
        None => Ok(0),
    }
}

/// Scans all entities that exceeded `MAX_TRIGRAMS_PER_DOC` (`TAG_OVERFLOW`) so queries never suffer false negatives.
pub fn scan_overflow_keys<R: KvRead>(txn: &R, db: SubDbHandle) -> Result<Vec<EntityKey>> {
    let mut out = Vec::new();
    let mut cur = txn.cursor(db)?;
    let mut step = cur.seek_ge(&[TAG_OVERFLOW])?;
    while let Some((k, _)) = step {
        if k.first() != Some(&TAG_OVERFLOW) {
            break;
        }
        if k.len() != 9 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated trigram overflow key"));
        }
        out.push(EntityKey::from_be_bytes(k[1..9].try_into().expect("8")));
        step = cur.next()?;
    }
    Ok(out)
}

/// Rarest-first intersection of required trigrams!
///
/// 1. Looks up `trig_df` for every required trigram in O(1); if any has `df == 0`, returns only `TAG_OVERFLOW` keys!
/// 2. Sorts required trigrams ascending by `df`.
/// 3. Loads blocks of the rarest trigram and intersects with remaining trigrams block-by-block, exiting early
///    as soon as the surviving candidate set becomes empty, then unions `TAG_OVERFLOW` keys for zero false negatives.
pub fn intersect_trigrams<R: KvRead>(txn: &R, db: SubDbHandle, required: &[String]) -> Result<Vec<EntityKey>> {
    if required.is_empty() {
        return Ok(Vec::new());
    }
    let overflow = scan_overflow_keys(txn, db)?;
    let mut ranked: Vec<(&str, u32)> = Vec::with_capacity(required.len());
    for t in required {
        let df = trigram_df(txn, db, t)?;
        if df == 0 {
            return Ok(overflow);
        }
        ranked.push((t.as_str(), df));
    }
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    ranked.dedup_by_key(|(t, _)| *t);

    // Scan blocks of the rarest trigram.
    let rarest = ranked[0].0;
    let mut prefix = Vec::with_capacity(2 + rarest.len());
    prefix.push(TAG_TRIGRAM);
    prefix.extend_from_slice(rarest.as_bytes());
    prefix.push(0x00);

    let mut blocks: Vec<(u64, RoaringBitmap)> = Vec::new();
    {
        let mut cur = txn.cursor(db)?;
        let mut step = cur.seek_ge(&prefix)?;
        while let Some((k, v)) = step {
            if k.len() != prefix.len() + 8 || !k.starts_with(&prefix) {
                break;
            }
            let block = u64::from_be_bytes(k[prefix.len()..prefix.len() + 8].try_into().expect("8"));
            if block >= (1u64 << 48) {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "trigram block exceeds 48-bit key space"));
            }
            blocks.push((block, decode_bitmap(v)?));
            step = cur.next()?;
        }
    }

    // Intersect remaining trigrams in ascending `df` order, dropping empty blocks early!
    for &(trig, _) in &ranked[1..] {
        if blocks.is_empty() {
            break;
        }
        let mut next_blocks = Vec::with_capacity(blocks.len());
        for (block, mut bm) in blocks {
            if let Some(raw) = txn.get(db, &trigram_key(trig, block))? {
                let other = decode_bitmap(raw)?;
                bm &= other;
                if !bm.is_empty() {
                    next_blocks.push((block, bm));
                }
            }
        }
        blocks = next_blocks;
    }

    let mut out = Vec::new();
    for (block, bm) in blocks {
        let base = block << 16;
        for off in bm.iter() {
            out.push(EntityKey(base | (off as u64)));
        }
    }
    if !overflow.is_empty() {
        out.extend(overflow);
        out.sort_unstable();
        out.dedup();
    }
    Ok(out)
}
