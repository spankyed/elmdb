//! Value and Unique secondary index projection (1 sub-DB per index, 1-byte table tags).
//!
//! Layout inside the index's pooled `SubDbSlot`:
//! - `0x01 || encoded_compound_fields...` ->
//!   - Inline (`count <= 256`): `[0x00, count(u16 LE), EntityKey(u64 LE) * count]` (sorted ascending)
//!   - Bucketed (`count > 256`): `[0x01, total_count(u64 LE)]`
//! - `0x05 || encoded_compound_fields... || bucket(u64 BE)` -> sorted `u8` offsets inside `[bucket<<8, (bucket+1)<<8)`
//!   (100% history-independent canonical representation: `derived == rebuild(primary)` byte-for-byte!)
//! - `0x02 || EntityKey(8B BE)` -> `()` (`Without`: missing/null indexed field)
//! - `0x03 || EntityKey(8B BE)` -> `()` (`Unordered`: unorderable type like Object/Array/Bytes)
//! - `0x04 || EntityKey(8B BE)` -> `()` (`Long`: string field > `MAX_INDEXED_STRING` = 448 B)

use elmdb_engine::{EntityKey, FieldId, PackedRow, SystemDbs, read_packed_row};
use elmdb_storage::{KvCursor, KvRead, SubDbHandle};
use elmdb_wire::{CanonicalValue, ElmdbError, ErrorCode, Result, encode_numeric_index_key};

use crate::state::KvMut;

/// Maximum indexed string byte length in a v2 value index (dedicated sub-DB leaves plenty of LMDB key headroom).
pub const MAX_INDEXED_STRING: usize = 448;
/// Maximum posting list entries stored inline in `TAG_MAIN` before spilling to 256-entity `TAG_BUCKET` blocks.
pub const INLINE_POSTING_LIMIT: usize = 256;

const TAG_MAIN: u8 = 0x01;
const TAG_WITHOUT: u8 = 0x02;
const TAG_UNORDERED: u8 = 0x03;
const TAG_LONG: u8 = 0x04;
const TAG_BUCKET: u8 = 0x05;

const POSTING_INLINE: u8 = 0x00;
const POSTING_BUCKETED: u8 = 0x01;

// Ordered field-part type tags (self-delimiting and lexicographically total-ordered).
const PART_BOOL: u8 = 0x01;
const PART_NUMERIC: u8 = 0x02; // Unified Int(i64) U finite Float(f64) (10 bytes!)
const PART_TIMESTAMP: u8 = 0x03; // i64 ms (ordered) + u32 ns (BE) = 12 bytes
const PART_ENTITY_REF: u8 = 0x04; // u64 BE = 8 bytes
const PART_STRING: u8 = 0x05; // zero-escaped bytes (`0x00 -> 0x00 0xFF`) + `0x00 0x00`
const PART_DANGLING_REF: u8 = 0x06; // zero-escaped bytes + `0x00 0x00`

/// Classification of an entity's compound key for a Value index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueKeyClass {
    Indexed(Vec<u8>),
    Without,
    Unordered,
    Long,
}

/// Encodes a single `CanonicalValue` as a self-delimiting, lexicographically ordered value-index part.
pub fn encode_value_part(val: &CanonicalValue, out: &mut Vec<u8>) -> ValuePartStatus {
    match val {
        CanonicalValue::Null => ValuePartStatus::Missing,
        CanonicalValue::Bool(b) => {
            out.push(PART_BOOL);
            out.push(u8::from(*b));
            ValuePartStatus::Ok
        }
        CanonicalValue::Int(_) | CanonicalValue::Float(_) => {
            let Some(num) = encode_numeric_index_key(val) else {
                return ValuePartStatus::Unordered;
            };
            out.push(PART_NUMERIC);
            out.extend_from_slice(&num);
            ValuePartStatus::Ok
        }
        CanonicalValue::Timestamp { ms, ns } => {
            out.push(PART_TIMESTAMP);
            let ordered_ms = (*ms as u64) ^ (1u64 << 63);
            out.extend_from_slice(&ordered_ms.to_be_bytes());
            out.extend_from_slice(&ns.to_be_bytes());
            ValuePartStatus::Ok
        }
        CanonicalValue::EntityRef(k) => {
            out.push(PART_ENTITY_REF);
            out.extend_from_slice(&k.to_be_bytes());
            ValuePartStatus::Ok
        }
        CanonicalValue::DanglingRef(s) => {
            if s.len() > MAX_INDEXED_STRING {
                return ValuePartStatus::Long;
            }
            out.push(PART_DANGLING_REF);
            encode_order_preserving_bytes(s.as_bytes(), out);
            ValuePartStatus::Ok
        }
        CanonicalValue::String(s) => {
            if s.len() > MAX_INDEXED_STRING {
                return ValuePartStatus::Long;
            }
            out.push(PART_STRING);
            encode_order_preserving_bytes(s.as_bytes(), out);
            ValuePartStatus::Ok
        }
        CanonicalValue::Bytes(_) | CanonicalValue::Array(_) | CanonicalValue::Object(_) => ValuePartStatus::Unordered,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValuePartStatus {
    Ok,
    Missing,
    Unordered,
    Long,
}

/// Order-preserving, self-delimiting byte encoding:
/// escapes `0x00 -> 0x00 0xFF` and terminates with `0x00 0x00`.
pub(crate) fn encode_order_preserving_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    for &b in bytes {
        if b == 0x00 {
            out.push(0x00);
            out.push(0xFF);
        } else {
            out.push(b);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

/// Encodes a compound tuple of values into the canonical index payload bytes (without table tag).
/// Deterministic precedence independent of field order: `Without > Unordered > Long > Indexed`.
pub fn encode_compound_values(vals: &[&CanonicalValue]) -> ValueKeyClass {
    let mut buf = Vec::with_capacity(vals.len() * 16);
    let mut has_unordered = false;
    let mut has_long = false;
    for v in vals {
        match encode_value_part(v, &mut buf) {
            ValuePartStatus::Ok => {}
            ValuePartStatus::Missing => return ValueKeyClass::Without,
            ValuePartStatus::Unordered => has_unordered = true,
            ValuePartStatus::Long => has_long = true,
        }
    }
    if has_unordered {
        return ValueKeyClass::Unordered;
    }
    if has_long || buf.len() > 502 {
        return ValueKeyClass::Long;
    }
    ValueKeyClass::Indexed(buf)
}

/// Classifies a live `PackedRow` for a Value index on `fields`.
/// Deterministic precedence independent of field order: `Without > Unordered > Long > Indexed`.
pub fn classify_row(row: &PackedRow, fields: &[FieldId]) -> ValueKeyClass {
    let mut buf = Vec::with_capacity(fields.len() * 16);
    let mut has_unordered = false;
    let mut has_long = false;
    for &fid in fields {
        let Some(val) = row.fields.get(&fid) else {
            return ValueKeyClass::Without;
        };
        match encode_value_part(val, &mut buf) {
            ValuePartStatus::Ok => {}
            ValuePartStatus::Missing => return ValueKeyClass::Without,
            ValuePartStatus::Unordered => has_unordered = true,
            ValuePartStatus::Long => has_long = true,
        }
    }
    if has_unordered {
        return ValueKeyClass::Unordered;
    }
    if has_long || buf.len() > 502 {
        return ValueKeyClass::Long;
    }
    ValueKeyClass::Indexed(buf)
}

fn main_key(payload: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + payload.len());
    k.push(TAG_MAIN);
    k.extend_from_slice(payload);
    k
}

fn bucket_key(payload: &[u8], bucket: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + payload.len() + 8);
    k.push(TAG_BUCKET);
    k.extend_from_slice(payload);
    k.extend_from_slice(&bucket.to_be_bytes());
    k
}

fn aux_key(tag: u8, key: EntityKey) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = tag;
    k[1..9].copy_from_slice(&key.to_be_bytes());
    k
}

fn encode_inline(keys: &[EntityKey]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(3 + keys.len() * 8);
    buf.push(POSTING_INLINE);
    buf.extend_from_slice(&(keys.len() as u16).to_le_bytes());
    for &ek in keys {
        buf.extend_from_slice(&ek.0.to_le_bytes());
    }
    buf
}

fn encode_bucketed_header(total_count: u64) -> [u8; 9] {
    let mut buf = [0u8; 9];
    buf[0] = POSTING_BUCKETED;
    buf[1..9].copy_from_slice(&total_count.to_le_bytes());
    buf
}

enum PostingHead {
    Inline(Vec<EntityKey>),
    Bucketed(u64),
}

fn decode_posting_head(raw: &[u8]) -> Result<PostingHead> {
    if raw.is_empty() {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "empty value posting"));
    }
    match raw[0] {
        POSTING_INLINE => {
            if raw.len() < 3 {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated inline posting"));
            }
            let count = u16::from_le_bytes([raw[1], raw[2]]) as usize;
            if raw.len() != 3 + count * 8 {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt inline posting length"));
            }
            let mut out = Vec::with_capacity(count);
            for i in 0..count {
                let off = 3 + i * 8;
                let k = u64::from_le_bytes(raw[off..off + 8].try_into().expect("8"));
                out.push(EntityKey(k));
            }
            Ok(PostingHead::Inline(out))
        }
        POSTING_BUCKETED => {
            if raw.len() != 9 {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt bucketed posting header"));
            }
            let total = u64::from_le_bytes(raw[1..9].try_into().expect("8"));
            Ok(PostingHead::Bucketed(total))
        }
        other => Err(ElmdbError::new(ErrorCode::Corrupted, format!("unknown posting mode {other}"))),
    }
}

fn read_all_bucketed_keys<R: KvRead>(txn: &R, db: SubDbHandle, payload: &[u8]) -> Result<Vec<EntityKey>> {
    let mut prefix = Vec::with_capacity(1 + payload.len());
    prefix.push(TAG_BUCKET);
    prefix.extend_from_slice(payload);
    let expected_len = prefix.len() + 8;
    let mut cur = txn.cursor(db)?;
    let mut out = Vec::new();
    let mut entry = cur.seek_ge(&prefix)?;
    while let Some((k, v)) = entry {
        if k.len() != expected_len || !k.starts_with(&prefix) {
            break;
        }
        let bucket = u64::from_be_bytes(k[prefix.len()..expected_len].try_into().expect("8"));
        let base = bucket << 8;
        for &off in v {
            out.push(EntityKey(base | (off as u64)));
        }
        entry = cur.next()?;
    }
    Ok(out)
}

fn insert_posting<W: KvMut>(
    txn: &mut W,
    db: SubDbHandle,
    payload: &[u8],
    key: EntityKey,
    unique: bool,
    index_name: &str,
) -> Result<()> {
    let mk = main_key(payload);
    let existing = txn.get(db, &mk)?.map(ToOwned::to_owned);
    match existing {
        None => {
            txn.put(db, &mk, &encode_inline(&[key]))?;
        }
        Some(raw) => match decode_posting_head(&raw)? {
            PostingHead::Inline(mut keys) => {
                match keys.binary_search(&key) {
                    Ok(_) => return Ok(()),
                    Err(pos) => {
                        if unique && !keys.is_empty() {
                            return Err(ElmdbError::new(
                                ErrorCode::UniqueViolation,
                                format!("unique index '{index_name}' violation"),
                            ));
                        }
                        keys.insert(pos, key);
                    }
                }
                if keys.len() <= INLINE_POSTING_LIMIT {
                    txn.put(db, &mk, &encode_inline(&keys))?;
                } else {
                    // Spill to canonical 256-entity aligned buckets.
                    let mut i = 0;
                    while i < keys.len() {
                        let b = keys[i].0 >> 8;
                        let mut offs = Vec::new();
                        while i < keys.len() && (keys[i].0 >> 8) == b {
                            offs.push((keys[i].0 & 0xFF) as u8);
                            i += 1;
                        }
                        txn.put(db, &bucket_key(payload, b), &offs)?;
                    }
                    txn.put(db, &mk, &encode_bucketed_header(keys.len() as u64))?;
                }
            }
            PostingHead::Bucketed(total) => {
                if unique && total > 0 {
                    // Check whether `key` is already the only member (unlikely in bucketed mode).
                    return Err(ElmdbError::new(
                        ErrorCode::UniqueViolation,
                        format!("unique index '{index_name}' violation"),
                    ));
                }
                let b = key.0 >> 8;
                let off = (key.0 & 0xFF) as u8;
                let bk = bucket_key(payload, b);
                let mut offs = txn.get(db, &bk)?.map(ToOwned::to_owned).unwrap_or_default();
                match offs.binary_search(&off) {
                    Ok(_) => return Ok(()),
                    Err(pos) => offs.insert(pos, off),
                }
                txn.put(db, &bk, &offs)?;
                txn.put(db, &mk, &encode_bucketed_header(total + 1))?;
            }
        },
    }
    Ok(())
}

fn remove_posting<W: KvMut>(txn: &mut W, db: SubDbHandle, payload: &[u8], key: EntityKey) -> Result<()> {
    let mk = main_key(payload);
    let Some(raw) = txn.get(db, &mk)?.map(ToOwned::to_owned) else {
        return Ok(());
    };
    match decode_posting_head(&raw)? {
        PostingHead::Inline(mut keys) => {
            if let Ok(pos) = keys.binary_search(&key) {
                keys.remove(pos);
                if keys.is_empty() {
                    txn.del(db, &mk)?;
                } else {
                    txn.put(db, &mk, &encode_inline(&keys))?;
                }
            }
        }
        PostingHead::Bucketed(total) => {
            let b = key.0 >> 8;
            let off = (key.0 & 0xFF) as u8;
            let bk = bucket_key(payload, b);
            let Some(mut offs) = txn.get(db, &bk)?.map(ToOwned::to_owned) else {
                return Ok(());
            };
            let Ok(pos) = offs.binary_search(&off) else {
                return Ok(());
            };
            offs.remove(pos);
            if offs.is_empty() {
                txn.del(db, &bk)?;
            } else {
                txn.put(db, &bk, &offs)?;
            }
            let new_total = total.saturating_sub(1);
            if new_total == 0 {
                txn.del(db, &mk)?;
            } else if new_total <= INLINE_POSTING_LIMIT as u64 {
                // Promote back to canonical inline representation and delete all buckets!
                let remaining = read_all_bucketed_keys(txn, db, payload)?;
                let mut buckets_to_del = Vec::new();
                for ek in &remaining {
                    let eb = ek.0 >> 8;
                    if buckets_to_del.last() != Some(&eb) {
                        buckets_to_del.push(eb);
                    }
                }
                for eb in buckets_to_del {
                    txn.del(db, &bucket_key(payload, eb))?;
                }
                txn.put(db, &mk, &encode_inline(&remaining))?;
            } else {
                txn.put(db, &mk, &encode_bucketed_header(new_total))?;
            }
        }
    }
    Ok(())
}

/// Borrowed specification for a Value/Unique index delta application.
#[derive(Debug, Clone, Copy)]
pub struct ValueIndexSpec<'a> {
    pub db: SubDbHandle,
    pub index_name: &'a str,
    pub fields: &'a [FieldId],
    pub unique: bool,
}

/// Removes `before`'s entry from a Value/Unique index (Pass 1 of atomic batch update).
pub fn remove_row<W: KvMut>(
    txn: &mut W,
    spec: ValueIndexSpec<'_>,
    key: EntityKey,
    before: Option<&PackedRow>,
) -> Result<()> {
    let Some(old) = before.map(|r| classify_row(r, spec.fields)) else {
        return Ok(());
    };
    match old {
        ValueKeyClass::Indexed(p) => remove_posting(txn, spec.db, &p, key)?,
        ValueKeyClass::Without => {
            txn.del(spec.db, &aux_key(TAG_WITHOUT, key))?;
        }
        ValueKeyClass::Unordered => {
            txn.del(spec.db, &aux_key(TAG_UNORDERED, key))?;
        }
        ValueKeyClass::Long => {
            txn.del(spec.db, &aux_key(TAG_LONG, key))?;
        }
    }
    Ok(())
}

/// Inserts `after`'s entry into a Value/Unique index (Pass 2 of atomic batch update, enforcing uniqueness).
pub fn insert_row<W: KvMut>(
    txn: &mut W,
    dbs: &SystemDbs,
    spec: ValueIndexSpec<'_>,
    key: EntityKey,
    after: Option<&PackedRow>,
) -> Result<()> {
    let Some(new) = after.map(|r| classify_row(r, spec.fields)) else {
        return Ok(());
    };
    match new {
        ValueKeyClass::Indexed(p) => insert_posting(txn, spec.db, &p, key, spec.unique, spec.index_name)?,
        ValueKeyClass::Without => {
            txn.put(spec.db, &aux_key(TAG_WITHOUT, key), &[])?;
        }
        ValueKeyClass::Unordered => {
            txn.put(spec.db, &aux_key(TAG_UNORDERED, key), &[])?;
        }
        ValueKeyClass::Long => {
            if spec.unique
                && let Some(row) = after
            {
                check_long_unique(txn, dbs, spec.db, spec.index_name, spec.fields, key, row)?;
            }
            txn.put(spec.db, &aux_key(TAG_LONG, key), &[])?;
        }
    }
    Ok(())
}

/// Applies an incremental `(before, after)` live-row delta to a Value/Unique index.
pub fn apply_delta<W: KvMut>(
    txn: &mut W,
    dbs: &SystemDbs,
    spec: ValueIndexSpec<'_>,
    key: EntityKey,
    before: Option<&PackedRow>,
    after: Option<&PackedRow>,
) -> Result<()> {
    let old_cls = before.map(|r| classify_row(r, spec.fields));
    let new_cls = after.map(|r| classify_row(r, spec.fields));
    if old_cls == new_cls && !matches!(new_cls, Some(ValueKeyClass::Long)) {
        return Ok(());
    }
    remove_row(txn, spec, key, before)?;
    insert_row(txn, dbs, spec, key, after)?;
    Ok(())
}

/// Ensures uniqueness is never bypassed by strings longer than `MAX_INDEXED_STRING` (448 B).
fn check_long_unique<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    db: SubDbHandle,
    index_name: &str,
    fields: &[FieldId],
    key: EntityKey,
    row: &PackedRow,
) -> Result<()> {
    let mut long_keys = Vec::new();
    {
        let mut cur = txn.cursor(db)?;
        let mut entry = cur.seek_ge(&[TAG_LONG])?;
        while let Some((k, _)) = entry {
            if k.len() != 9 || k[0] != TAG_LONG {
                break;
            }
            let other = EntityKey::from_be_bytes(k[1..9].try_into().expect("8"));
            if other != key {
                long_keys.push(other);
            }
            entry = cur.next()?;
        }
    }
    for other in long_keys {
        if let Some(other_row) = read_packed_row(txn, dbs, other)? {
            let same = fields.iter().all(|fid| row.fields.get(fid) == other_row.fields.get(fid));
            if same {
                return Err(ElmdbError::new(
                    ErrorCode::UniqueViolation,
                    format!("unique index '{index_name}' violation on long string value"),
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Query read helpers (consumed by `elmdb-query`)
// ---------------------------------------------------------------------------

/// Exact equality lookup returning all matching `EntityKey`s in ascending order.
pub fn lookup_eq<R: KvRead>(txn: &R, db: SubDbHandle, vals: &[&CanonicalValue]) -> Result<Vec<EntityKey>> {
    let ValueKeyClass::Indexed(payload) = encode_compound_values(vals) else {
        return Ok(Vec::new());
    };
    let Some(raw) = txn.get(db, &main_key(&payload))? else {
        return Ok(Vec::new());
    };
    match decode_posting_head(raw)? {
        PostingHead::Inline(keys) => Ok(keys),
        PostingHead::Bucketed(_) => read_all_bucketed_keys(txn, db, &payload),
    }
}

/// O(1) exact equality count pushdown (reads only the posting header, zero row/bucket reads!).
pub fn count_eq<R: KvRead>(txn: &R, db: SubDbHandle, vals: &[&CanonicalValue]) -> Result<u64> {
    let ValueKeyClass::Indexed(payload) = encode_compound_values(vals) else {
        return Ok(0);
    };
    let Some(raw) = txn.get(db, &main_key(&payload))? else {
        return Ok(0);
    };
    match decode_posting_head(raw)? {
        PostingHead::Inline(keys) => Ok(keys.len() as u64),
        PostingHead::Bucketed(total) => Ok(total),
    }
}

/// Returns the smallest byte vector strictly greater than every byte slice starting with `prefix`.
fn next_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.pop() {
        if last < u8::MAX {
            out.push(last + 1);
            return Some(out);
        }
    }
    None
}

/// Bounded bucketed-key reader (stops after `limit` keys, respecting `reverse` order!).
fn read_bucketed_keys_bounded<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    payload: &[u8],
    reverse: bool,
    limit: usize,
) -> Result<Vec<EntityKey>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut prefix = Vec::with_capacity(1 + payload.len());
    prefix.push(TAG_BUCKET);
    prefix.extend_from_slice(payload);
    let expected_len = prefix.len() + 8;
    let mut cur = txn.cursor(db)?;
    let mut out = Vec::with_capacity(limit.min(4096));

    let mut step = if !reverse {
        cur.seek_ge(&prefix)?
    } else {
        match next_prefix(&prefix) {
            Some(upper) => match cur.seek_ge(&upper)? {
                Some(_) => cur.prev()?,
                None => cur.last()?,
            },
            None => cur.last()?,
        }
    };

    while let Some((k, v)) = step {
        if k.len() != expected_len || !k.starts_with(&prefix) {
            break;
        }
        let bucket = u64::from_be_bytes(k[prefix.len()..expected_len].try_into().expect("8"));
        let base = bucket << 8;
        if !reverse {
            for &off in v {
                out.push(EntityKey(base | (off as u64)));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
            step = cur.next()?;
        } else {
            for &off in v.iter().rev() {
                out.push(EntityKey(base | (off as u64)));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
            step = cur.prev()?;
        }
    }
    Ok(out)
}

/// Returns all `EntityKey`s stored in the `TAG_LONG` partition (strings > 448 B / compound keys > 502 B)
/// so query executors never silently miss long-string matches.
pub fn scan_long_keys<R: KvRead>(txn: &R, db: SubDbHandle) -> Result<Vec<EntityKey>> {
    scan_aux_partition(txn, db, TAG_LONG)
}

/// Returns all `EntityKey`s stored in `TAG_WITHOUT` (missing/null indexed fields).
pub fn scan_without_keys<R: KvRead>(txn: &R, db: SubDbHandle) -> Result<Vec<EntityKey>> {
    scan_aux_partition(txn, db, TAG_WITHOUT)
}

/// Returns all `EntityKey`s stored in `TAG_UNORDERED`.
pub fn scan_unordered_keys<R: KvRead>(txn: &R, db: SubDbHandle) -> Result<Vec<EntityKey>> {
    scan_aux_partition(txn, db, TAG_UNORDERED)
}

fn scan_aux_partition<R: KvRead>(txn: &R, db: SubDbHandle, tag: u8) -> Result<Vec<EntityKey>> {
    let mut cur = txn.cursor(db)?;
    let mut out = Vec::new();
    let mut step = cur.seek_ge(&[tag])?;
    while let Some((k, _)) = step {
        if k.first() != Some(&tag) {
            break;
        }
        if k.len() != 9 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt aux partition key length"));
        }
        out.push(EntityKey::from_be_bytes(k[1..9].try_into().expect("8")));
        step = cur.next()?;
    }
    Ok(out)
}

/// Range scan over `TAG_MAIN` returning matching `EntityKey`s in index order (ascending or descending).
///
/// - Handles both full-arity bounds and compound prefix bounds via self-delimiting `next_prefix`.
/// - When one bound is open (`None`) and the other is `Some`, clamps the open side to the provided
///   bound's first-field type tag (`PART_*`) so `Any` fields never bleed across distinct scalar types.
pub fn scan_range<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    low: Option<(&[&CanonicalValue], bool)>,
    high: Option<(&[&CanonicalValue], bool)>,
    reverse: bool,
    limit: Option<usize>,
) -> Result<Vec<EntityKey>> {
    let cap = limit.unwrap_or(usize::MAX);
    if cap == 0 {
        return Ok(Vec::new());
    }
    let low_enc = match low {
        Some((vals, inc)) => match encode_compound_values(vals) {
            ValueKeyClass::Indexed(p) => Some((main_key(&p), inc)),
            _ => return Ok(Vec::new()),
        },
        None => None,
    };
    let high_enc = match high {
        Some((vals, inc)) => match encode_compound_values(vals) {
            ValueKeyClass::Indexed(p) => Some((main_key(&p), inc)),
            _ => return Ok(Vec::new()),
        },
        None => None,
    };

    // Convert (key, inclusive) into half-open `[start_inc, end_exc)` byte boundaries so compound
    // prefixes work identically to exact-arity bounds, and clamp open sides to the same first-field type tag!
    let start_inc: Vec<u8> = match (&low_enc, &high_enc) {
        (Some((lb, true)), _) => lb.clone(),
        (Some((lb, false)), _) => match next_prefix(lb) {
            Some(np) => np,
            None => return Ok(Vec::new()),
        },
        (None, Some((hb, _))) if hb.len() >= 2 => vec![TAG_MAIN, hb[1]],
        (None, _) => vec![TAG_MAIN],
    };

    let end_exc: Vec<u8> = match (&high_enc, &low_enc) {
        (Some((hb, true)), _) => match next_prefix(hb) {
            Some(np) => np,
            None => vec![TAG_MAIN + 1],
        },
        (Some((hb, false)), _) => hb.clone(),
        (None, Some((lb, _))) if lb.len() >= 2 => {
            if lb[1] < u8::MAX {
                vec![TAG_MAIN, lb[1] + 1]
            } else {
                vec![TAG_MAIN + 1]
            }
        }
        (None, _) => vec![TAG_MAIN + 1],
    };

    if start_inc >= end_exc {
        return Ok(Vec::new());
    }

    let mut entries: Vec<(Vec<u8>, PostingHead)> = Vec::new();
    let mut collected = 0usize;
    {
        let mut cur = txn.cursor(db)?;
        let mut step = if !reverse {
            cur.seek_ge(&start_inc)?
        } else {
            match cur.seek_ge(&end_exc)? {
                Some(_) => cur.prev()?,
                None => cur.last()?,
            }
        };

        while let Some((k, v)) = step {
            if k < start_inc.as_slice() || k >= end_exc.as_slice() {
                break;
            }
            let head = decode_posting_head(v)?;
            let count = match &head {
                PostingHead::Inline(ks) => ks.len(),
                PostingHead::Bucketed(t) => (*t).min(usize::MAX as u64) as usize,
            };
            entries.push((k[1..].to_vec(), head));
            collected = collected.saturating_add(count);
            if collected >= cap {
                break;
            }
            step = if reverse { cur.prev()? } else { cur.next()? };
        }
    }

    let mut out = Vec::with_capacity(collected.min(cap).min(4096));
    for (payload, head) in entries {
        let remaining = cap - out.len();
        if remaining == 0 {
            break;
        }
        match head {
            PostingHead::Inline(mut ks) => {
                if reverse {
                    ks.reverse();
                }
                out.extend(ks.into_iter().take(remaining));
            }
            PostingHead::Bucketed(_) => {
                out.extend(read_bucketed_keys_bounded(txn, db, &payload, reverse, remaining)?);
            }
        }
    }
    Ok(out)
}

/// Scans entities whose first indexed string field starts with `prefix`.
pub fn scan_prefix_string<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    prefix: &str,
    limit: Option<usize>,
) -> Result<Vec<EntityKey>> {
    if prefix.len() > MAX_INDEXED_STRING {
        return Ok(Vec::new());
    }
    let cap = limit.unwrap_or(usize::MAX);
    if cap == 0 {
        return Ok(Vec::new());
    }
    let mut key_prefix = Vec::with_capacity(2 + prefix.len());
    key_prefix.push(TAG_MAIN);
    key_prefix.push(PART_STRING);
    for &b in prefix.as_bytes() {
        if b == 0x00 {
            key_prefix.push(0x00);
            key_prefix.push(0xFF);
        } else {
            key_prefix.push(b);
        }
        // Respect LMDB's 511-byte key ceiling (1B TAG_MAIN + 502B payload ceiling = 503B).
        if key_prefix.len() > 503 {
            return Ok(Vec::new());
        }
    }

    let mut entries = Vec::new();
    let mut total = 0usize;
    {
        let mut cur = txn.cursor(db)?;
        let mut step = cur.seek_ge(&key_prefix)?;
        while let Some((k, v)) = step {
            if !k.starts_with(&key_prefix) {
                break;
            }
            let head = decode_posting_head(v)?;
            let count = match &head {
                PostingHead::Inline(ks) => ks.len(),
                PostingHead::Bucketed(t) => (*t).min(usize::MAX as u64) as usize,
            };
            entries.push((k[1..].to_vec(), head));
            total = total.saturating_add(count);
            if total >= cap {
                break;
            }
            step = cur.next()?;
        }
    }
    let mut out = Vec::with_capacity(total.min(cap).min(4096));
    for (payload, head) in entries {
        let remaining = cap - out.len();
        if remaining == 0 {
            break;
        }
        match head {
            PostingHead::Inline(ks) => out.extend(ks.into_iter().take(remaining)),
            PostingHead::Bucketed(_) => {
                out.extend(read_bucketed_keys_bounded(txn, db, &payload, false, remaining)?);
            }
        }
    }
    Ok(out)
}
