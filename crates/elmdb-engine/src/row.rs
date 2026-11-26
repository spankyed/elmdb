//! Hybrid Packed-Row + 4 KiB out-of-line blob encoding (`PackedRow`) and `PackedEdge`.
//!
//! Layout guarantees:
//! - Single `MDB_INTEGERKEY` lookup reads all row metadata + inline fields (`<= 4 KiB` each,
//!   total inline payload capped at `16 KiB` by spilling largest fields first to `blobs`).
//! - `FieldDir` is sorted ascending by `FieldId` for O(log F) single-field access without
//!   decoding sibling fields.
//! - Whole-row zstd compression activates at `>= 256 B` payload, capturing cross-field redundancy.

use std::collections::BTreeMap;

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

const FLAG_TRASHED: u16 = 1 << 0;
const FLAG_ROW_ZSTD: u16 = 1 << 1;
const FLAG_HAS_BLOBS: u16 = 1 << 2;
const FLAG_HAS_EXTRA: u16 = 1 << 3;

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

    // 1. Canonical-encode every declared field and identify initial > 4 KiB spills.
    let mut encoded_fields: Vec<(FieldId, Vec<u8>, bool)> = Vec::with_capacity(row.fields.len());
    let mut inline_total = 0usize;
    for (&fid, val) in &row.fields {
        let mut bytes = Vec::new();
        encode_value(val, &mut bytes);
        let spill = bytes.len() > INLINE_BLOB_THRESHOLD;
        if !spill {
            inline_total += bytes.len();
        }
        encoded_fields.push((fid, bytes, spill));
    }

    // 2. Enforce 16 KiB total inline row cap by spilling largest inline fields first.
    if inline_total > MAX_INLINE_ROW_BYTES {
        let mut order: Vec<usize> =
            encoded_fields.iter().enumerate().filter(|(_, (_, _, spilled))| !*spilled).map(|(i, _)| i).collect();
        order.sort_by(|&a, &b| encoded_fields[b].1.len().cmp(&encoded_fields[a].1.len()));
        for idx in order {
            if inline_total <= MAX_INLINE_ROW_BYTES {
                break;
            }
            inline_total -= encoded_fields[idx].1.len();
            encoded_fields[idx].2 = true;
        }
    }

    let mut blobs = Vec::new();
    let mut blob_field_ids = Vec::new();

    // 3. Build uncompressed payload: [FieldDir; N] [ExtraFields?] [InlineValues...]
    let dir_bytes = encoded_fields.len() * 8;
    let mut extra_buf = Vec::new();
    if !row.extra.is_empty() {
        extra_buf.extend_from_slice(&(row.extra.len() as u32).to_le_bytes());
        for (k, v) in &row.extra {
            extra_buf.extend_from_slice(&(k.len() as u32).to_le_bytes());
            extra_buf.extend_from_slice(k.as_bytes());
            encode_value(v, &mut extra_buf);
        }
    }

    let mut values_buf = Vec::with_capacity(inline_total);
    let mut dir_buf = Vec::with_capacity(dir_bytes);
    let values_base = dir_bytes + if extra_buf.is_empty() { 0 } else { 4 + extra_buf.len() };

    for (fid, bytes, spilled) in encoded_fields {
        if spilled {
            // Blob value format: 0x01 || zstd(bytes) if compressed smaller, else 0x00 || raw.
            let blob_val = compress_blob(&bytes)?;
            blobs.push((fid, blob_val));
            blob_field_ids.push(fid);
            dir_buf.extend_from_slice(&fid.0.to_le_bytes());
            dir_buf.push(0); // reserved tag
            dir_buf.push(FIELD_FLAG_BLOB);
            dir_buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        } else {
            let offset = (values_base + values_buf.len()) as u32;
            values_buf.extend_from_slice(&bytes);
            dir_buf.extend_from_slice(&fid.0.to_le_bytes());
            dir_buf.push(0);
            dir_buf.push(0);
            dir_buf.extend_from_slice(&offset.to_le_bytes());
        }
    }

    let mut payload = Vec::with_capacity(values_base + values_buf.len());
    payload.extend_from_slice(&dir_buf);
    if !extra_buf.is_empty() {
        payload.extend_from_slice(&(extra_buf.len() as u32).to_le_bytes());
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

    // 4. Whole-row zstd compression when payload >= 256 B and compressed shrinks it.
    let final_payload = if payload.len() >= ROW_ZSTD_THRESHOLD {
        match zstd::bulk::compress(&payload, 1) {
            Ok(compressed) if compressed.len() + 4 < payload.len() => {
                flags |= FLAG_ROW_ZSTD;
                let mut framed = Vec::with_capacity(4 + compressed.len());
                framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
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
    row_bytes.extend_from_slice(&(row.fields.len() as u16).to_le_bytes());
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

fn decompress_payload(flags: u16, raw: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    if (flags & FLAG_ROW_ZSTD) == 0 {
        return Ok(std::borrow::Cow::Borrowed(raw));
    }
    if raw.len() < 4 {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated zstd row payload"));
    }
    let orig_len = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
    let decompressed = zstd::bulk::decompress(&raw[4..], orig_len)
        .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, format!("zstd row decompress failed: {e}")))?;
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
    // Binary search 8-byte FieldDir entries sorted ascending by FieldId.
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
                        ElmdbError::new(ErrorCode::Corrupted, format!("missing blob for field {:?}", target))
                    })?;
                    let decompressed = decompress_blob(&raw_blob)?;
                    let (val, _) = decode_value(&decompressed)?;
                    return Ok(Some(val));
                }
                if val_off >= payload.len() {
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
            decode_value(&decompressed)?.0
        } else {
            if val_off >= payload.len() {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "FieldDir offset out of bounds"));
            }
            decode_value(&payload[val_off..])?.0
        };
        fields.insert(fid, val);
    }

    let mut extra = BTreeMap::new();
    if (header.flags & FLAG_HAS_EXTRA) != 0 {
        if payload.len() < dir_end + 4 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated ExtraFields length"));
        }
        let extra_len = u32::from_le_bytes(payload[dir_end..dir_end + 4].try_into().expect("4")) as usize;
        let extra_slice = payload
            .get(dir_end + 4..dir_end + 4 + extra_len)
            .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "truncated ExtraFields payload"))?;
        let mut pos = 0usize;
        if extra_slice.len() < 4 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated ExtraFields count"));
        }
        let extra_count = u32::from_le_bytes(extra_slice[0..4].try_into().expect("4")) as usize;
        pos += 4;
        for _ in 0..extra_count {
            if pos + 4 > extra_slice.len() {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated extra key len"));
            }
            let klen = u32::from_le_bytes(extra_slice[pos..pos + 4].try_into().expect("4")) as usize;
            pos += 4;
            let key_bytes = extra_slice
                .get(pos..pos + klen)
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "truncated extra key"))?;
            let key = std::str::from_utf8(key_bytes)
                .map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "invalid UTF-8 extra key"))?
                .to_owned();
            pos += klen;
            let (val, consumed) = decode_value(&extra_slice[pos..])?;
            pos += consumed;
            extra.insert(key, val);
        }
    }

    Ok(PackedRow { header, fields, extra })
}

/// Returns the list of `FieldId`s currently stored out-of-line in `blobs` for this row.
pub fn blob_field_ids_of(bytes: &[u8]) -> Result<Vec<FieldId>> {
    let (header, payload_offset) = decode_row_header(bytes)?;
    if !header.has_blobs() {
        return Ok(Vec::new());
    }
    let payload = decompress_payload(header.flags, &bytes[payload_offset..])?;
    let count = usize::from(header.field_count);
    let mut out = Vec::new();
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
        let mut out = Vec::with_capacity(5 + compressed.len());
        out.push(0x01);
        out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
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
            zstd::bulk::decompress(&stored[5..], orig_len)
                .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, format!("blob zstd decompress failed: {e}")))
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
        let mut out = Vec::with_capacity(48);
        out.extend_from_slice(&self.kind.0.to_le_bytes());
        out.extend_from_slice(&self.src.0.to_le_bytes());
        out.extend_from_slice(&self.dst.0.to_le_bytes());
        out.extend_from_slice(&self.weight.to_bits().to_le_bytes());
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
        let created_at_ms = i64::from_le_bytes(bytes[26..34].try_into().expect("8"));
        let info = match bytes[34] {
            0 => None,
            1 => Some(decode_value(&bytes[35..])?.0),
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
        // Single-field probe on FieldId(3) never touches blob_map!
        let f3 = decode_single_field(&encoded.row_bytes, FieldId(3), |_| panic!("should not read blob")).unwrap();
        assert_eq!(f3, Some(CanonicalValue::Int(42)));

        let decoded = decode_row(&encoded.row_bytes, |fid| Ok(blob_map.get(&fid).cloned())).unwrap();
        assert_eq!(decoded.fields.get(&FieldId(2)), Some(&CanonicalValue::String(big)));
        assert_eq!(decoded.extra.get("dyn_key"), Some(&CanonicalValue::Bool(true)));
    }
}
