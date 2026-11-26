//! Single-pass binary batch & result wire framing (`elmdb-wire`).
//!
//! - Write frames support intra-batch entity references (`EntityTarget::BatchSlot(u32)`),
//!   allowing `create + link` flows inside one N-API crossing without dropping to `transact()`.
//! - Read/result frames carry a per-response string dictionary interning field names and
//!   repeated resolved `EntityKey -> ext_id` strings (`FxHashMap<u64, u16>`), eliminating
//!   full-buffer `__proto__` scans on both Rust and JS sides.

use rustc_hash::FxHashMap;

use crate::error::{ElmdbError, ErrorCode, Result};
use crate::value::{CanonicalValue, decode_value, encode_value};

const BATCH_MAGIC: [u8; 4] = *b"EWB2";
const RESULT_MAGIC: [u8; 4] = *b"EWR2";

/// Bitflags controlling what data `execute_batch` serializes back to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommitReturnFlags(u8);

impl CommitReturnFlags {
    pub const NONE: Self = Self(0);
    pub const RESULTS: Self = Self(1 << 0);
    pub const TOUCHED_TOKENS: Self = Self(1 << 1);
    pub const FULL_CHANGESET: Self = Self(1 << 2);

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, flag: Self) -> bool {
        (self.0 & flag.0) == flag.0
    }
}

/// Target entity inside a wire operation: either an external string ID or a 0-based slot
/// referencing an entity created earlier in the same batch frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityTarget {
    ExternalId(String),
    BatchSlot(u32),
}

/// One typed write operation encoded in a batch wire frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireOp {
    Create {
        type_name: String,
        external_id: Option<String>,
        fields: Vec<(String, CanonicalValue)>,
    },
    Patch {
        target: EntityTarget,
        fields: Vec<(String, CanonicalValue)>,
    },
    Unset {
        target: EntityTarget,
        fields: Vec<String>,
    },
    Trash {
        target: EntityTarget,
    },
    Restore {
        target: EntityTarget,
    },
    Destroy {
        target: EntityTarget,
    },
    Link {
        source: EntityTarget,
        rel_kind: String,
        target: EntityTarget,
        weight: Option<u64>, // f64 bits
        info: Option<CanonicalValue>,
    },
    Unlink {
        source: EntityTarget,
        rel_kind: Option<String>,
        target: Option<EntityTarget>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedBatch {
    pub return_flags: CommitReturnFlags,
    pub ops: Vec<WireOp>,
}

/// Encodes a batch of write operations into a single contiguous binary wire frame.
pub fn encode_batch(return_flags: CommitReturnFlags, ops: &[WireOp]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + ops.len() * 32);
    out.extend_from_slice(&BATCH_MAGIC);
    out.push(return_flags.bits());
    write_u32(ops.len(), &mut out);
    for op in ops {
        match op {
            WireOp::Create { type_name, external_id, fields } => {
                out.push(1);
                write_str(type_name, &mut out);
                match external_id {
                    Some(id) => {
                        out.push(1);
                        write_str(id, &mut out);
                    }
                    None => out.push(0),
                }
                write_field_pairs(fields, &mut out);
            }
            WireOp::Patch { target, fields } => {
                out.push(2);
                write_target(target, &mut out);
                write_field_pairs(fields, &mut out);
            }
            WireOp::Unset { target, fields } => {
                out.push(3);
                write_target(target, &mut out);
                write_u32(fields.len(), &mut out);
                for f in fields {
                    write_str(f, &mut out);
                }
            }
            WireOp::Trash { target } => {
                out.push(4);
                write_target(target, &mut out);
            }
            WireOp::Restore { target } => {
                out.push(5);
                write_target(target, &mut out);
            }
            WireOp::Destroy { target } => {
                out.push(6);
                write_target(target, &mut out);
            }
            WireOp::Link { source, rel_kind, target, weight, info } => {
                out.push(7);
                write_target(source, &mut out);
                write_str(rel_kind, &mut out);
                write_target(target, &mut out);
                match weight {
                    Some(bits) => {
                        out.push(1);
                        out.extend_from_slice(&bits.to_le_bytes());
                    }
                    None => out.push(0),
                }
                match info {
                    Some(v) => {
                        out.push(1);
                        encode_value(v, &mut out);
                    }
                    None => out.push(0),
                }
            }
            WireOp::Unlink { source, rel_kind, target } => {
                out.push(8);
                write_target(source, &mut out);
                match rel_kind {
                    Some(k) => {
                        out.push(1);
                        write_str(k, &mut out);
                    }
                    None => out.push(0),
                }
                match target {
                    Some(t) => {
                        out.push(1);
                        write_target(t, &mut out);
                    }
                    None => out.push(0),
                }
            }
        }
    }
    out
}

/// Decodes a binary batch frame in one single pass.
pub fn decode_batch(buf: &[u8]) -> Result<DecodedBatch> {
    let mut r = SliceReader::new(buf);
    if r.take(4)? != BATCH_MAGIC {
        return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "bad batch wire magic"));
    }
    let raw_flags = r.read_u8()?;
    let valid_mask = CommitReturnFlags::RESULTS.bits()
        | CommitReturnFlags::TOUCHED_TOKENS.bits()
        | CommitReturnFlags::FULL_CHANGESET.bits();
    if (raw_flags & !valid_mask) != 0 {
        return Err(ElmdbError::new(
            ErrorCode::InvalidWireFrame,
            format!("undefined CommitReturnFlags bits {raw_flags:#04x}"),
        ));
    }
    let return_flags = CommitReturnFlags::from_bits(raw_flags);
    let op_count = r.read_u32()? as usize;
    if op_count > buf.len() {
        return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "corrupt batch op count"));
    }
    let mut ops = Vec::with_capacity(op_count.min(256));
    for _ in 0..op_count {
        let tag = r.read_u8()?;
        let op = match tag {
            1 => {
                let type_name = r.read_str()?.to_owned();
                let external_id = match r.read_u8()? {
                    0 => None,
                    1 => Some(r.read_str()?.to_owned()),
                    _ => return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "bad opt tag")),
                };
                let fields = read_field_pairs(&mut r)?;
                WireOp::Create { type_name, external_id, fields }
            }
            2 => {
                let target = read_target(&mut r)?;
                let fields = read_field_pairs(&mut r)?;
                WireOp::Patch { target, fields }
            }
            3 => {
                let target = read_target(&mut r)?;
                let count = r.read_u32()? as usize;
                let mut fields = Vec::with_capacity(count.min(64));
                for _ in 0..count {
                    fields.push(r.read_str()?.to_owned());
                }
                WireOp::Unset { target, fields }
            }
            4 => WireOp::Trash { target: read_target(&mut r)? },
            5 => WireOp::Restore { target: read_target(&mut r)? },
            6 => WireOp::Destroy { target: read_target(&mut r)? },
            7 => {
                let source = read_target(&mut r)?;
                let rel_kind = r.read_str()?.to_owned();
                let target = read_target(&mut r)?;
                let weight = match r.read_u8()? {
                    0 => None,
                    1 => Some(r.read_u64()?),
                    _ => return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "bad weight opt tag")),
                };
                let info = match r.read_u8()? {
                    0 => None,
                    1 => Some(r.read_value()?),
                    _ => return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "bad info opt tag")),
                };
                WireOp::Link { source, rel_kind, target, weight, info }
            }
            8 => {
                let source = read_target(&mut r)?;
                let rel_kind = match r.read_u8()? {
                    0 => None,
                    1 => Some(r.read_str()?.to_owned()),
                    _ => return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "bad rel_kind opt tag")),
                };
                let target = match r.read_u8()? {
                    0 => None,
                    1 => Some(read_target(&mut r)?),
                    _ => return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "bad target opt tag")),
                };
                WireOp::Unlink { source, rel_kind, target }
            }
            other => {
                return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, format!("unknown wire op tag {other}")));
            }
        };
        ops.push(op);
    }
    if r.pos != buf.len() {
        return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "trailing bytes in batch frame"));
    }
    Ok(DecodedBatch { return_flags, ops })
}

/// Encodes multi-row query results using a per-response string dictionary for field names
/// AND a per-response `EntityKey -> dict_id` cache so repeated parent references resolve once.
#[derive(Debug, Default)]
pub struct ResultEncoder {
    dict: Vec<String>,
    str_to_id: FxHashMap<String, u16>,
    entity_to_dict_id: FxHashMap<u64, u16>,
    body: Vec<u8>,
    row_count: u32,
}

impl ResultEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, s: &str) -> Result<u16> {
        if let Some(&id) = self.str_to_id.get(s) {
            return Ok(id);
        }
        let id = u16::try_from(self.dict.len())
            .map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "response string dictionary > 65535"))?;
        self.dict.push(s.to_owned());
        self.str_to_id.insert(s.to_owned(), id);
        Ok(id)
    }

    /// Resolves `entity_key` via `resolve_fn` at most once per response and interns its external ID.
    pub fn intern_entity_ref<F>(&mut self, entity_key: u64, resolve_fn: F) -> Result<u16>
    where
        F: FnOnce(u64) -> Result<String>,
    {
        if let Some(&id) = self.entity_to_dict_id.get(&entity_key) {
            return Ok(id);
        }
        let ext_id = resolve_fn(entity_key)?;
        let dict_id = self.intern(&ext_id)?;
        self.entity_to_dict_id.insert(entity_key, dict_id);
        Ok(dict_id)
    }

    /// Appends one row `(external_id, type_name, fields)` to the response frame atomically
    /// (truncating partial body bytes on error) and resolving nested `EntityRef`s in arrays.
    pub fn push_row<F>(
        &mut self,
        external_id: &str,
        type_name: &str,
        fields: &[(&str, &CanonicalValue)],
        mut resolve_ref: F,
    ) -> Result<()>
    where
        F: FnMut(u64) -> Result<String>,
    {
        let next_count = self
            .row_count
            .checked_add(1)
            .ok_or_else(|| ElmdbError::new(ErrorCode::ResourceLimit, "response row count overflow"))?;
        let checkpoint = self.body.len();
        let res = (|| -> Result<()> {
            let id_sym = self.intern(external_id)?;
            let type_sym = self.intern(type_name)?;
            self.body.extend_from_slice(&id_sym.to_le_bytes());
            self.body.extend_from_slice(&type_sym.to_le_bytes());
            let field_count = u16::try_from(fields.len())
                .map_err(|_| ElmdbError::new(ErrorCode::ResourceLimit, "row field count > 65535"))?;
            self.body.extend_from_slice(&field_count.to_le_bytes());
            for (name, val) in fields {
                let sym = self.intern(name)?;
                self.body.extend_from_slice(&sym.to_le_bytes());
                match val {
                    CanonicalValue::EntityRef(key) => {
                        let ref_sym = self.intern_entity_ref(*key, &mut resolve_ref)?;
                        self.body.push(0xFE); // interned external ID reference
                        self.body.extend_from_slice(&ref_sym.to_le_bytes());
                    }
                    CanonicalValue::DanglingRef(ext) => {
                        let ref_sym = self.intern(ext)?;
                        self.body.push(0xFE);
                        self.body.extend_from_slice(&ref_sym.to_le_bytes());
                    }
                    other if other.contains_internal_ref() => {
                        let resolved = resolve_value_refs(other, &mut resolve_ref)?;
                        self.body.push(0xFF);
                        encode_value(&resolved, &mut self.body);
                    }
                    other => {
                        self.body.push(0xFF); // raw canonical value
                        encode_value(other, &mut self.body);
                    }
                }
            }
            Ok(())
        })();
        if let Err(err) = res {
            self.body.truncate(checkpoint);
            return Err(err);
        }
        self.row_count = next_count;
        Ok(())
    }

    /// Finalizes the frame: `[MAGIC:4][dict_len:2][dict_strings...][row_count:4][body...]`.
    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.dict.len() * 12 + self.body.len());
        out.extend_from_slice(&RESULT_MAGIC);
        out.extend_from_slice(&(self.dict.len() as u16).to_le_bytes());
        for s in &self.dict {
            write_u32(s.len(), &mut out);
            out.extend_from_slice(s.as_bytes());
        }
        out.extend_from_slice(&self.row_count.to_le_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRow {
    pub id: String,
    pub type_name: String,
    pub fields: Vec<(String, CanonicalValue)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedResultFrame {
    pub dict: Vec<String>,
    pub has_proto_key: bool,
    pub rows: Vec<DecodedRow>,
}

/// Decodes a `ResultEncoder` binary frame, checking `__proto__` strictly once per unique
/// dictionary string (`O(unique_keys)` instead of scanning the entire response buffer).
pub struct ResultDecoder;

impl ResultDecoder {
    pub fn decode(buf: &[u8]) -> Result<DecodedResultFrame> {
        let mut r = SliceReader::new(buf);
        if r.take(4)? != RESULT_MAGIC {
            return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "bad result wire magic"));
        }
        let dict_len = usize::from(r.read_u16()?);
        let mut dict = Vec::with_capacity(dict_len.min(256));
        let mut has_proto_key = false;
        for _ in 0..dict_len {
            let s = r.read_str()?;
            if s == "__proto__" {
                has_proto_key = true;
            }
            dict.push(s.to_owned());
        }
        let row_count = r.read_u32()? as usize;
        let mut rows = Vec::with_capacity(row_count.min(256));
        for _ in 0..row_count {
            let id_sym = usize::from(r.read_u16()?);
            let type_sym = usize::from(r.read_u16()?);
            let id = dict
                .get(id_sym)
                .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidWireFrame, "bad id dict index"))?
                .clone();
            let type_name = dict
                .get(type_sym)
                .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidWireFrame, "bad type dict index"))?
                .clone();
            let field_count = usize::from(r.read_u16()?);
            let mut fields = Vec::with_capacity(field_count.min(64));
            for _ in 0..field_count {
                let name_sym = usize::from(r.read_u16()?);
                let name = dict
                    .get(name_sym)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidWireFrame, "bad field dict index"))?
                    .clone();
                let val = match r.read_u8()? {
                    0xFE => {
                        let ref_sym = usize::from(r.read_u16()?);
                        let ext = dict
                            .get(ref_sym)
                            .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidWireFrame, "bad ref dict index"))?
                            .clone();
                        CanonicalValue::String(ext)
                    }
                    0xFF => {
                        let v = r.read_value()?;
                        if !has_proto_key && v.contains_proto_key() {
                            has_proto_key = true;
                        }
                        v
                    }
                    other => {
                        return Err(ElmdbError::new(
                            ErrorCode::InvalidWireFrame,
                            format!("bad result value mode {other:#04x}"),
                        ));
                    }
                };
                fields.push((name, val));
            }
            rows.push(DecodedRow { id, type_name, fields });
        }
        if r.pos != buf.len() {
            return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "trailing bytes in result frame"));
        }
        Ok(DecodedResultFrame { dict, has_proto_key, rows })
    }
}

fn resolve_value_refs<F>(val: &CanonicalValue, resolve_ref: &mut F) -> Result<CanonicalValue>
where
    F: FnMut(u64) -> Result<String>,
{
    match val {
        CanonicalValue::EntityRef(k) => Ok(CanonicalValue::String(resolve_ref(*k)?)),
        CanonicalValue::DanglingRef(ext) => Ok(CanonicalValue::String(ext.clone())),
        CanonicalValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(resolve_value_refs(item, resolve_ref)?);
            }
            Ok(CanonicalValue::Array(out))
        }
        CanonicalValue::Object(entries) => {
            let mut out = Vec::with_capacity(entries.len());
            for (k, v) in entries {
                out.push((k.clone(), resolve_value_refs(v, resolve_ref)?));
            }
            Ok(CanonicalValue::Object(out))
        }
        other => Ok(other.clone()),
    }
}

fn write_u32(n: usize, out: &mut Vec<u8>) {
    let v = u32::try_from(n).expect("wire len fits u32");
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_str(s: &str, out: &mut Vec<u8>) {
    write_u32(s.len(), out);
    out.extend_from_slice(s.as_bytes());
}

fn write_target(t: &EntityTarget, out: &mut Vec<u8>) {
    match t {
        EntityTarget::ExternalId(id) => {
            out.push(0);
            write_str(id, out);
        }
        EntityTarget::BatchSlot(slot) => {
            out.push(1);
            out.extend_from_slice(&slot.to_le_bytes());
        }
    }
}

fn write_field_pairs(fields: &[(String, CanonicalValue)], out: &mut Vec<u8>) {
    write_u32(fields.len(), out);
    for (k, v) in fields {
        write_str(k, out);
        encode_value(v, out);
    }
}

fn read_target(r: &mut SliceReader<'_>) -> Result<EntityTarget> {
    match r.read_u8()? {
        0 => Ok(EntityTarget::ExternalId(r.read_str()?.to_owned())),
        1 => Ok(EntityTarget::BatchSlot(r.read_u32()?)),
        other => Err(ElmdbError::new(ErrorCode::InvalidWireFrame, format!("bad EntityTarget tag {other}"))),
    }
}

fn read_field_pairs(r: &mut SliceReader<'_>) -> Result<Vec<(String, CanonicalValue)>> {
    let count = r.read_u32()? as usize;
    let mut fields = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let k = r.read_str()?.to_owned();
        let v = r.read_value()?;
        fields.push((k, v));
    }
    Ok(fields)
}

struct SliceReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> SliceReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidWireFrame, "wire offset overflow"))?;
        if end > self.buf.len() {
            return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "unexpected EOF in wire frame"));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes")))
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn read_str(&mut self) -> Result<&'a str> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map_err(|_| ElmdbError::new(ErrorCode::InvalidWireFrame, "invalid UTF-8 in wire frame"))
    }

    fn read_value(&mut self) -> Result<CanonicalValue> {
        let (val, consumed) = decode_value(&self.buf[self.pos..])?;
        self.pos += consumed;
        Ok(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_roundtrips_intra_batch_slots() {
        let ops = vec![
            WireOp::Create {
                type_name: "Task".into(),
                external_id: None,
                fields: vec![("title".into(), CanonicalValue::String("Ship v2".into()))],
            },
            WireOp::Link {
                source: EntityTarget::BatchSlot(0),
                rel_kind: "IN".into(),
                target: EntityTarget::ExternalId("Folder-1".into()),
                weight: Some(1.5f64.to_bits()),
                info: None,
            },
        ];
        let flags = CommitReturnFlags::RESULTS.union(CommitReturnFlags::TOUCHED_TOKENS);
        let bytes = encode_batch(flags, &ops);
        let decoded = decode_batch(&bytes).unwrap();
        assert_eq!(decoded.return_flags, flags);
        assert_eq!(decoded.ops, ops);
    }

    #[test]
    fn result_encoder_interns_field_names_and_caches_repeated_entity_refs() {
        let mut enc = ResultEncoder::new();
        let mut lookups = 0u32;
        let parent_ref = CanonicalValue::EntityRef(77);
        let title = CanonicalValue::String("Hello".into());

        for i in 0..10 {
            let id = format!("Task-{i}");
            enc.push_row(&id, "Task", &[("title", &title), ("parent", &parent_ref)], |k| {
                lookups += 1;
                assert_eq!(k, 77);
                Ok("Project-main".to_owned())
            })
            .unwrap();
        }
        // 10 rows referencing EntityKey(77) triggered ONLY 1 resolver lookup!
        assert_eq!(lookups, 1);

        let frame = enc.finish();
        let decoded = ResultDecoder::decode(&frame).unwrap();
        assert!(!decoded.has_proto_key);
        assert_eq!(decoded.rows.len(), 10);
        assert_eq!(decoded.rows[9].fields[1], ("parent".into(), CanonicalValue::String("Project-main".into())));
    }
}
