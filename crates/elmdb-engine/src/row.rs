//! Hybrid Packed-Row + 4 KiB out-of-line blob encoding (`PackedRow`) and `PackedEdge`.
//!
//! Layout guarantees:
//! - Single `MDB_INTEGERKEY` lookup reads all row metadata + inline fields (`<= 4 KiB` each,
//!   total inline payload capped at `16 KiB` by spilling largest fields first to `blobs`).
//! - `ExtraFields` (`additionalFields: true`) also respects the 4 KiB spill ceiling, spilling
//!   to `blobs` under the reserved `EXTRA_BLOB_FIELD_ID = FieldId(0)`.
//! - `FieldDir` is sorted ascending by `FieldId` for O(log F) single-field access without
//!   decoding sibling fields.
//! - Bounded streaming zstd decompression prevents 4 GiB upfront-allocation OOM bombs on
//!   corrupted length headers.

use std::collections::BTreeMap;
use std::io::Read;

use elmdb_wire::{CanonicalValue, ElmdbError, ErrorCode, Result, decode_value, encode_value};

use crate::ids::{EntityKey, FieldId, RelKey, RelKindId, TypeId};

/// Maximum UTF-8 byte length of an external entity ID (`ext_id` sub-DB ceiling).
pub const MAX_EXT_ID_BYTES: usize = 256;
/// Individual field values strictly larger than 4 KiB spill to the `blobs` sub-DB.
pub const INLINE_BLOB_THRESHOLD: usize = 4096;
/// Total inline payload ceiling per row; largest inline fields spill first if exceeded.
pub const MAX_INLINE_ROW_BYTES: usize = 16_384;
/// Whole-row zstd compression threshold.
pub const ROW_ZSTD_THRESHOLD: usize = 256;
/// Hard ceiling for any single decompressed row payload or out-of-line blob (64 MiB).
pub const MAX_DECOMPRESSED_BYTES: usize = 64 * 1024 * 1024;
/// Reserved `FieldId(0)` used in `blobs` when `ExtraFields` exceeds `INLINE_BLOB_THRESHOLD`.
/// Declared `FieldId`s start at `1`, so `0` can never collide.
pub const EXTRA_BLOB_FIELD_ID: FieldId = FieldId(0);

const FLAG_TRASHED: u16 = 1 << 0;
const FLAG_ROW_ZSTD: u16 = 1 << 1;
const FLAG_HAS_BLOBS: u16 = 1 << 2;
const FLAG_HAS_EXTRA: u16 = 1 << 3;
const FLAG_EXTRA_SPILLED: u16 = 1 << 4;

const FIELD_FLAG_BLOB: u8 = 1 << 0;

/// Fixed 40-byte header at the front of every `PackedRow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowHeader {
    pub type_id: TypeId,
    pub flags: u16,
    pub field_count: u16,
    pub ext_id: String,
    pub updated_seq: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub trashed_at_ms: i64,
}

impl RowHeader {
    pub fn is_trashed(&self) -> bool {
        (self.flags & FLAG_TRASHED) != 0
    }

    pub fn has_blobs(&self) -> bool {
        (self.flags & FLAG_HAS_BLOBS) != 0
    }

    pub fn has_extra(&self) -> bool {
        (self.flags & FLAG_HAS_EXTRA) != 0
    }

    pub fn extra_spilled(&self) -> bool {
        (self.flags & FLAG_EXTRA_SPILLED) != 0
    }
}

/// Decoded logical representation of a stored entity row (declared + optional extra fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedRow {
    pub header: RowHeader,
    pub fields: BTreeMap<FieldId, CanonicalValue>,
    pub extra: BTreeMap<String, CanonicalValue>,
}

/// Output of encoding a `PackedRow`: compact inline row bytes plus any fields spilled to `blobs`.
#[derive(Debug, Clone)]
pub struct EncodedRow {
    pub row_bytes: Vec<u8>,
    pub blobs: Vec<(FieldId, Vec<u8>)>,
    pub blob_field_ids: Vec<FieldId>,
}

/// Encodes `row` into hybrid packed-row bytes + spilled `blobs` payloads.
pub fn encode_row(row: &PackedRow) -> Result<EncodedRow> {
    if row.header.ext_id.is_empty() || row.header.ext_id.len() > MAX_EXT_ID_BYTES {
        return Err(ElmdbError::new(
            ErrorCode::InvalidArgument,
            format!("external ID length {} out of 1..={MAX_EXT_ID_BYTES}", row.header.ext_id.len()),
        ));
    }

    let field_count_u16 = u16::try_from(row.fields.len())
        .map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "declared field count > 65535"))?;

    // 1. Canonical-encode every declared field and identify initial > 4 KiB spills.
    let mut encoded_fields: Vec<(FieldId, Vec<u8>, bool)> = Vec::with_capacity(row.fields.len());
    let mut inline_total = 0usize;
    for (&fid, val) in &row.fields {
        let mut bytes = Vec::new();
        encode_value(val, &mut bytes);
        if bytes.len() > MAX_DECOMPRESSED_BYTES {
            return Err(ElmdbError::new(ErrorCode::ResourceLimit, "field value exceeds 64 MiB limit"));
        }
        let spill = bytes.len() > INLINE_BLOB_THRESHOLD;
        if !spill {
            inline_total = inline_total.saturating_add(bytes.len());
        }
        encoded_fields.push((fid, bytes, spill));
    }

    // 2. Encode ExtraFields and spill to `EXTRA_BLOB_FIELD_ID` if > 4 KiB.
    let mut extra_buf = Vec::new();
    let mut extra_spilled = false;
    if !row.extra.is_empty() {
        let extra_cnt = u32::try_from(row.extra.len())
            .map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "extra field count overflow"))?;
        extra_buf.extend_from_slice(&extra_cnt.to_le_bytes());
        for (k, v) in &row.extra {
            let klen = u32::try_from(k.len())
                .map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "extra key length overflow"))?;
            extra_buf.extend_from_slice(&klen.to_le_bytes());
            extra_buf.extend_from_slice(k.as_bytes());
            encode_value(v, &mut extra_buf);
        }
        if extra_buf.len() > MAX_DECOMPRESSED_BYTES {
            return Err(ElmdbError::new(ErrorCode::ResourceLimit, "extra fields exceed 64 MiB limit"));
        }
        if extra_buf.len() > INLINE_BLOB_THRESHOLD {
            extra_spilled = true;
        } else {
            inline_total = inline_total.saturating_add(extra_buf.len());
        }
    }

    // 3. Enforce 16 KiB total inline row cap by spilling largest inline declared fields first.
    if inline_total > MAX_INLINE_ROW_BYTES {
        let mut order: Vec<usize> =
            encoded_fields.iter().enumerate().filter(|(_, (_, _, spilled))| !*spilled).map(|(i, _)| i).collect();
        order.sort_by(|&a, &b| encoded_fields[b].1.len().cmp(&encoded_fields[a].1.len()));
        for idx in order {
            if inline_total <= MAX_INLINE_ROW_BYTES {
                break;
            }
            inline_total = inline_total.saturating_sub(encoded_fields[idx].1.len());
            encoded_fields[idx].2 = true;
        }
        if inline_total > MAX_INLINE_ROW_BYTES && !extra_buf.is_empty() && !extra_spilled {
            extra_spilled = true;
        }
    }

    let mut blobs = Vec::new();
    let mut blob_field_ids = Vec::new();

    if extra_spilled {
        blobs.push((EXTRA_BLOB_FIELD_ID, compress_blob(&extra_buf)?));
        blob_field_ids.push(EXTRA_BLOB_FIELD_ID);
    }

    let dir_bytes = encoded_fields.len() * 8;
    let inline_extra_bytes = if !extra_buf.is_empty() && !extra_spilled { 4 + extra_buf.len() } else { 0 };
    let values_base = dir_bytes + inline_extra_bytes;

    let mut values_buf = Vec::with_capacity(inline_total);
    let mut dir_buf = Vec::with_capacity(dir_bytes);

    for (fid, bytes, spilled) in encoded_fields {
        if spilled {
            let blob_val = compress_blob(&bytes)?;
            blobs.push((fid, blob_val));
            blob_field_ids.push(fid);
            dir_buf.extend_from_slice(&fid.0.to_le_bytes());
            dir_buf.push(0);
            dir_buf.push(FIELD_FLAG_BLOB);
            let blen = u32::try_from(bytes.len())
                .map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "blob length > u32"))?;
            dir_buf.extend_from_slice(&blen.to_le_bytes());
        } else {
            let offset = u32::try_from(values_base + values_buf.len())
                .map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "inline row offset > u32"))?;
            values_buf.extend_from_slice(&bytes);
            dir_buf.extend_from_slice(&fid.0.to_le_bytes());
            dir_buf.push(0);
            dir_buf.push(0);
            dir_buf.extend_from_slice(&offset.to_le_bytes());
        }
    }

    let mut payload = Vec::with_capacity(values_base + values_buf.len());
    payload.extend_from_slice(&dir_buf);
    if !extra_buf.is_empty() && !extra_spilled {
        let elen = u32::try_from(extra_buf.len())
            .map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "extra payload > u32"))?;
        payload.extend_from_slice(&elen.to_le_bytes());
        payload.extend_from_slice(&extra_buf);
    }
    payload.extend_from_slice(&values_buf);

    let mut flags = row.header.flags & FLAG_TRASHED;
    if !blobs.is_empty() {
        flags |= FLAG_HAS_BLOBS;
    }
    if !row.extra.is_empty() {
        flags |= FLAG_HAS_EXTRA;
    }
    if extra_spilled {
        flags |= FLAG_EXTRA_SPILLED;
    }

    let final_payload = if payload.len() >= ROW_ZSTD_THRESHOLD {
        match zstd::bulk::compress(&payload, 1) {
            Ok(compressed) if compressed.len() + 4 < payload.len() => {
                flags |= FLAG_ROW_ZSTD;
                let orig = u32::try_from(payload.len())
                    .map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "row payload > u32"))?;
                let mut framed = Vec::with_capacity(4 + compressed.len());
                framed.extend_from_slice(&orig.to_le_bytes());
                framed.extend_from_slice(&compressed);
                framed
            }
            _ => payload,
        }
    } else {
        payload
    };

    let ext_bytes = row.header.ext_id.as_bytes();
    let mut row_bytes = Vec::with_capacity(40 + ext_bytes.len() + final_payload.len());
    row_bytes.extend_from_slice(&row.header.type_id.0.to_le_bytes());
    row_bytes.extend_from_slice(&flags.to_le_bytes());
    row_bytes.extend_from_slice(&field_count_u16.to_le_bytes());
    row_bytes.extend_from_slice(&(ext_bytes.len() as u16).to_le_bytes());
    row_bytes.extend_from_slice(&row.header.updated_seq.to_le_bytes());
    row_bytes.extend_from_slice(&row.header.created_at_ms.to_le_bytes());
    row_bytes.extend_from_slice(&row.header.updated_at_ms.to_le_bytes());
    row_bytes.extend_from_slice(&row.header.trashed_at_ms.to_le_bytes());
    row_bytes.extend_from_slice(ext_bytes);
    row_bytes.extend_from_slice(&final_payload);

    Ok(EncodedRow { row_bytes, blobs, blob_field_ids })
}

/// Decodes ONLY the fixed header + `ext_id` of a `PackedRow` (zero decompression, zero field decoding!).
pub fn decode_row_header(bytes: &[u8]) -> Result<(RowHeader, usize)> {
    if bytes.len() < 40 {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated PackedRow header"));
    }
    let type_id = TypeId(u16::from_le_bytes([bytes[0], bytes[1]]));
    let flags = u16::from_le_bytes([bytes[2], bytes[3]]);
    let field_count = u16::from_le_bytes([bytes[4], bytes[5]]);
    let ext_len = usize::from(u16::from_le_bytes([bytes[6], bytes[7]]));
    if ext_len == 0 || ext_len > MAX_EXT_ID_BYTES {
        return Err(ElmdbError::new(
            ErrorCode::Corrupted,
            format!("corrupt external_id length {ext_len} in PackedRow"),
        ));
    }
    let updated_seq = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
    let created_at_ms = i64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes"));
    let updated_at_ms = i64::from_le_bytes(bytes[24..32].try_into().expect("8 bytes"));
    let trashed_at_ms = i64::from_le_bytes(bytes[32..40].try_into().expect("8 bytes"));

    let payload_offset = 40 + ext_len;
    if bytes.len() < payload_offset {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated PackedRow external_id"));
    }
    let ext_id = std::str::from_utf8(&bytes[40..payload_offset])
        .map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "invalid UTF-8 external_id in PackedRow"))?
        .to_owned();

    Ok((
        RowHeader { type_id, flags, field_count, ext_id, updated_seq, created_at_ms, updated_at_ms, trashed_at_ms },
        payload_offset,
    ))
}

/// Bounded streaming zstd decompression: never trusts `expected_len` for upfront allocation
/// (defeats 4 GiB OOM bomb headers) and enforces `decompressed.len() == expected_len <= max_bytes`.
fn decompress_zstd_bounded(compressed: &[u8], expected_len: usize, max_bytes: usize) -> Result<Vec<u8>> {
    if expected_len > max_bytes {
        return Err(ElmdbError::new(
            ErrorCode::Corrupted,
            format!("zstd header length {expected_len} exceeds max {max_bytes}"),
        ));
    }
    let decoder = zstd::stream::Decoder::new(compressed)
        .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, format!("zstd decoder init failed: {e}")))?;
    let limit = (expected_len as u64).saturating_add(1);
    let mut out = Vec::with_capacity(expected_len.min(64 * 1024));
    decoder
        .take(limit)
        .read_to_end(&mut out)
        .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, format!("zstd decompress failed: {e}")))?;
    if out.len() != expected_len {
        return Err(ElmdbError::new(
            ErrorCode::Corrupted,
            format!("zstd decompressed length {} != header {expected_len}", out.len()),
        ));
    }
    Ok(out)
}

fn decompress_payload(flags: u16, raw: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    if (flags & FLAG_ROW_ZSTD) == 0 {
        return Ok(std::borrow::Cow::Borrowed(raw));
    }
    if raw.len() < 4 {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated zstd row payload"));
    }
    let orig_len = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
    let decompressed = decompress_zstd_bounded(&raw[4..], orig_len, MAX_DECOMPRESSED_BYTES)?;
    Ok(std::borrow::Cow::Owned(decompressed))
}

/// Decodes a single declared field via O(log F) binary search in `FieldDir` without decoding siblings!
pub fn decode_single_field<F>(bytes: &[u8], target: FieldId, mut load_blob: F) -> Result<Option<CanonicalValue>>
where
    F: FnMut(FieldId) -> Result<Option<Vec<u8>>>,
{
    let (header, payload_offset) = decode_row_header(bytes)?;
    let payload = decompress_payload(header.flags, &bytes[payload_offset..])?;
    let count = usize::from(header.field_count);
    let dir_end = count * 8;
    if payload.len() < dir_end {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated FieldDir"));
    }
    let dir = &payload[..dir_end];
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let off = mid * 8;
        let fid = u16::from_le_bytes([dir[off], dir[off + 1]]);
        match fid.cmp(&target.0) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => {
                let fflags = dir[off + 3];
                let val_off = u32::from_le_bytes([dir[off + 4], dir[off + 5], dir[off + 6], dir[off + 7]]) as usize;
                if (fflags & FIELD_FLAG_BLOB) != 0 {
                    let raw_blob = load_blob(target)?.ok_or_else(|| {
                        ElmdbError::new(ErrorCode::Corrupted, format!("missing blob for field {target:?}"))
                    })?;
                    let decompressed = decompress_blob(&raw_blob)?;
                    let (val, consumed) = decode_value(&decompressed)?;
                    if consumed != decompressed.len() {
                        return Err(ElmdbError::new(ErrorCode::Corrupted, "trailing bytes in blob value"));
                    }
                    return Ok(Some(val));
                }
                if val_off < dir_end || val_off >= payload.len() {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "FieldDir offset out of bounds"));
                }
                let (val, _) = decode_value(&payload[val_off..])?;
                return Ok(Some(val));
            }
        }
    }
    Ok(None)
}

/// Decodes a complete `PackedRow`, loading any spilled blobs via `load_blob`.
pub fn decode_row<F>(bytes: &[u8], mut load_blob: F) -> Result<PackedRow>
where
    F: FnMut(FieldId) -> Result<Option<Vec<u8>>>,
{
    let (header, payload_offset) = decode_row_header(bytes)?;
    let payload = decompress_payload(header.flags, &bytes[payload_offset..])?;
    let count = usize::from(header.field_count);
    let dir_end = count * 8;
    if payload.len() < dir_end {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated FieldDir"));
    }

    let mut extra = BTreeMap::new();
    let values_base = if header.has_extra() {
        if header.extra_spilled() {
            let raw_blob = load_blob(EXTRA_BLOB_FIELD_ID)?
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "missing spilled ExtraFields blob"))?;
            let extra_slice = decompress_blob(&raw_blob)?;
            decode_extra_slice(&extra_slice, &mut extra)?;
            dir_end
        } else {
            let len_end = dir_end
                .checked_add(4)
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "extra len offset overflow"))?;
            if payload.len() < len_end {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated ExtraFields length"));
            }
            let extra_len = u32::from_le_bytes(payload[dir_end..len_end].try_into().expect("4")) as usize;
            let extra_end = len_end
                .checked_add(extra_len)
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "extra payload offset overflow"))?;
            let extra_slice = payload
                .get(len_end..extra_end)
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "truncated ExtraFields payload"))?;
            decode_extra_slice(extra_slice, &mut extra)?;
            extra_end
        }
    } else {
        dir_end
    };

    let mut fields = BTreeMap::new();
    for i in 0..count {
        let off = i * 8;
        let fid = FieldId(u16::from_le_bytes([payload[off], payload[off + 1]]));
        let fflags = payload[off + 3];
        let val_off =
            u32::from_le_bytes([payload[off + 4], payload[off + 5], payload[off + 6], payload[off + 7]]) as usize;
        let val = if (fflags & FIELD_FLAG_BLOB) != 0 {
            let raw_blob = load_blob(fid)?
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, format!("missing blob for field {fid:?}")))?;
            let decompressed = decompress_blob(&raw_blob)?;
            let (v, consumed) = decode_value(&decompressed)?;
            if consumed != decompressed.len() {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "trailing bytes in blob value"));
            }
            v
        } else {
            if val_off < values_base || val_off >= payload.len() {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "FieldDir offset out of bounds"));
            }
            decode_value(&payload[val_off..])?.0
        };
        fields.insert(fid, val);
    }

    Ok(PackedRow { header, fields, extra })
}

fn decode_extra_slice(extra_slice: &[u8], out: &mut BTreeMap<String, CanonicalValue>) -> Result<()> {
    if extra_slice.len() < 4 {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated ExtraFields count"));
    }
    let extra_count = u32::from_le_bytes(extra_slice[0..4].try_into().expect("4")) as usize;
    let mut pos = 4usize;
    for _ in 0..extra_count {
        let klen_end =
            pos.checked_add(4).ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "extra key len overflow"))?;
        if klen_end > extra_slice.len() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated extra key len"));
        }
        let klen = u32::from_le_bytes(extra_slice[pos..klen_end].try_into().expect("4")) as usize;
        pos = klen_end;
        let key_end =
            pos.checked_add(klen).ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "extra key end overflow"))?;
        let key_bytes = extra_slice
            .get(pos..key_end)
            .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "truncated extra key"))?;
        let key = std::str::from_utf8(key_bytes)
            .map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "invalid UTF-8 extra key"))?
            .to_owned();
        pos = key_end;
        let (val, consumed) = decode_value(&extra_slice[pos..])?;
        pos = pos
            .checked_add(consumed)
            .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "extra value offset overflow"))?;
        out.insert(key, val);
    }
    if pos != extra_slice.len() {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "trailing bytes in ExtraFields"));
    }
    Ok(())
}

/// Returns the list of `FieldId`s (including `EXTRA_BLOB_FIELD_ID` if spilled) stored in `blobs`.
pub fn blob_field_ids_of(bytes: &[u8]) -> Result<Vec<FieldId>> {
    let (header, payload_offset) = decode_row_header(bytes)?;
    if !header.has_blobs() {
        return Ok(Vec::new());
    }
    let payload = decompress_payload(header.flags, &bytes[payload_offset..])?;
    let count = usize::from(header.field_count);
    let mut out = Vec::new();
    if header.extra_spilled() {
        out.push(EXTRA_BLOB_FIELD_ID);
    }
    for i in 0..count {
        let off = i * 8;
        if off + 8 > payload.len() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated FieldDir"));
        }
        if (payload[off + 3] & FIELD_FLAG_BLOB) != 0 {
            out.push(FieldId(u16::from_le_bytes([payload[off], payload[off + 1]])));
        }
    }
    Ok(out)
}

pub fn blob_key(entity: EntityKey, field: FieldId) -> [u8; 10] {
    let mut k = [0u8; 10];
    k[0..8].copy_from_slice(&entity.to_be_bytes());
    k[8..10].copy_from_slice(&field.0.to_be_bytes());
    k
}

fn compress_blob(raw: &[u8]) -> Result<Vec<u8>> {
    if let Ok(compressed) = zstd::bulk::compress(raw, 1)
        && compressed.len() + 5 < raw.len() + 1
    {
        let orig =
            u32::try_from(raw.len()).map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "blob exceeds u32"))?;
        let mut out = Vec::with_capacity(5 + compressed.len());
        out.push(0x01);
        out.extend_from_slice(&orig.to_le_bytes());
        out.extend_from_slice(&compressed);
        return Ok(out);
    }
    let mut out = Vec::with_capacity(1 + raw.len());
    out.push(0x00);
    out.extend_from_slice(raw);
    Ok(out)
}

fn decompress_blob(stored: &[u8]) -> Result<Vec<u8>> {
    if stored.is_empty() {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "empty blob record"));
    }
    match stored[0] {
        0x00 => Ok(stored[1..].to_vec()),
        0x01 => {
            if stored.len() < 5 {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated compressed blob"));
            }
            let orig_len = u32::from_le_bytes([stored[1], stored[2], stored[3], stored[4]]) as usize;
            decompress_zstd_bounded(&stored[5..], orig_len, MAX_DECOMPRESSED_BYTES)
        }
        other => Err(ElmdbError::new(ErrorCode::Corrupted, format!("unknown blob tag {other}"))),
    }
}

/// Primary edge record stored in `relations` (`MDB_INTEGERKEY`: `RelKey(u64)`).
#[derive(Debug, Clone, PartialEq)]
pub struct PackedEdge {
    pub key: RelKey,
    pub kind: RelKindId,
    pub src: EntityKey,
    pub dst: EntityKey,
    pub weight: f64,
    pub created_at_ms: i64,
    pub info: Option<CanonicalValue>,
}

impl PackedEdge {
    pub fn encode(&self) -> Vec<u8> {
        let w = if self.weight == 0.0 { 0.0 } else { self.weight };
        let mut out = Vec::with_capacity(48);
        out.extend_from_slice(&self.kind.0.to_le_bytes());
        out.extend_from_slice(&self.src.0.to_le_bytes());
        out.extend_from_slice(&self.dst.0.to_le_bytes());
        out.extend_from_slice(&w.to_bits().to_le_bytes());
        out.extend_from_slice(&self.created_at_ms.to_le_bytes());
        match &self.info {
            Some(v) => {
                out.push(1);
                encode_value(v, &mut out);
            }
            None => out.push(0),
        }
        out
    }

    pub fn decode(key: RelKey, bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 35 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated PackedEdge"));
        }
        let kind = RelKindId(u16::from_le_bytes([bytes[0], bytes[1]]));
        let src = EntityKey(u64::from_le_bytes(bytes[2..10].try_into().expect("8")));
        let dst = EntityKey(u64::from_le_bytes(bytes[10..18].try_into().expect("8")));
        let weight = f64::from_bits(u64::from_le_bytes(bytes[18..26].try_into().expect("8")));
        if !weight.is_finite() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "non-finite weight in PackedEdge"));
        }
        let created_at_ms = i64::from_le_bytes(bytes[26..34].try_into().expect("8"));
        let info = match bytes[34] {
            0 => {
                if bytes.len() != 35 {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "trailing bytes in PackedEdge"));
                }
                None
            }
            1 => {
                let (v, consumed) = decode_value(&bytes[35..])?;
                if 35 + consumed != bytes.len() {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "trailing bytes in PackedEdge info"));
                }
                Some(v)
            }
            other => {
                return Err(ElmdbError::new(ErrorCode::Corrupted, format!("bad PackedEdge info tag {other}")));
            }
        };
        Ok(Self { key, kind, src, dst, weight, created_at_ms, info })
    }
}

pub fn make_trashed_flags(trashed: bool) -> u16 {
    if trashed { FLAG_TRASHED } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_row_spills_blobs_over_4kb_and_supports_single_field_lookup() {
        let big = "x".repeat(5000);
        let mut fields = BTreeMap::new();
        fields.insert(FieldId(1), CanonicalValue::String("short".into()));
        fields.insert(FieldId(2), CanonicalValue::String(big.clone()));
        fields.insert(FieldId(3), CanonicalValue::Int(42));
        let mut extra = BTreeMap::new();
        extra.insert("dyn_key".into(), CanonicalValue::Bool(true));

        let row = PackedRow {
            header: RowHeader {
                type_id: TypeId(1),
                flags: 0,
                field_count: 3,
                ext_id: "Doc-1".into(),
                updated_seq: 7,
                created_at_ms: 100,
                updated_at_ms: 200,
                trashed_at_ms: 0,
            },
            fields,
            extra,
        };

        let encoded = encode_row(&row).unwrap();
        assert_eq!(encoded.blob_field_ids, vec![FieldId(2)]);
        assert!(encoded.row_bytes.len() < 256);

        let blob_map: BTreeMap<FieldId, Vec<u8>> = encoded.blobs.into_iter().collect();
        let f3 = decode_single_field(&encoded.row_bytes, FieldId(3), |_| panic!("should not read blob")).unwrap();
        assert_eq!(f3, Some(CanonicalValue::Int(42)));

        let decoded = decode_row(&encoded.row_bytes, |fid| Ok(blob_map.get(&fid).cloned())).unwrap();
        assert_eq!(decoded.fields.get(&FieldId(2)), Some(&CanonicalValue::String(big)));
        assert_eq!(decoded.extra.get("dyn_key"), Some(&CanonicalValue::Bool(true)));
    }

    #[test]
    fn large_extra_fields_spill_and_zstd_bomb_header_is_rejected_without_oom() {
        let mut extra = BTreeMap::new();
        extra.insert("huge".into(), CanonicalValue::String("Z".repeat(6000)));
        let row = PackedRow {
            header: RowHeader {
                type_id: TypeId(1),
                flags: 0,
                field_count: 0,
                ext_id: "Doc-2".into(),
                updated_seq: 1,
                created_at_ms: 0,
                updated_at_ms: 0,
                trashed_at_ms: 0,
            },
            fields: BTreeMap::new(),
            extra,
        };
        let encoded = encode_row(&row).unwrap();
        assert!(encoded.blob_field_ids.contains(&EXTRA_BLOB_FIELD_ID));
        let blob_map: BTreeMap<FieldId, Vec<u8>> = encoded.blobs.into_iter().collect();
        let decoded = decode_row(&encoded.row_bytes, |fid| Ok(blob_map.get(&fid).cloned())).unwrap();
        assert_eq!(decoded.extra.get("huge"), Some(&CanonicalValue::String("Z".repeat(6000))));

        // Corrupt zstd length header to u32::MAX -> rejected cleanly with Corrupted (never OOMs!).
        let fake_bomb = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
        let err = decompress_blob(&fake_bomb).unwrap_err();
        assert_eq!(err.code, ErrorCode::Corrupted);
    }
}
