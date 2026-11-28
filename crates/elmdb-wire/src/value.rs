//! Canonical typed value representation (`CanonicalValue` / zero-copy `ValueRef<'a>`)
//! and exact unified numeric index key encoding (`Int(i64)` ∪ finite `Float(f64)`).

use std::cmp::Ordering;

use crate::error::{ElmdbError, ErrorCode, Result};

/// Fixed length of a unified numeric index key (`[sortable_f64: 8B][biased_delta_be: 2B]`).
pub const NUMERIC_INDEX_KEY_LEN: usize = 10;

/// Maximum nesting depth allowed when decoding values from untrusted wire frames.
pub const MAX_VALUE_DEPTH: usize = 64;

/// Discriminant tags for canonical binary value encoding (shared by wire & `PackedRow`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ValueTag {
    Null = 0,
    BoolFalse = 1,
    BoolTrue = 2,
    Int = 3,
    Float = 4,
    Timestamp = 5,
    EntityRef = 6,
    DanglingRef = 7,
    String = 8,
    Bytes = 9,
    Array = 10,
    Object = 11,
}

impl ValueTag {
    pub fn from_u8(b: u8) -> Result<Self> {
        match b {
            0 => Ok(Self::Null),
            1 => Ok(Self::BoolFalse),
            2 => Ok(Self::BoolTrue),
            3 => Ok(Self::Int),
            4 => Ok(Self::Float),
            5 => Ok(Self::Timestamp),
            6 => Ok(Self::EntityRef),
            7 => Ok(Self::DanglingRef),
            8 => Ok(Self::String),
            9 => Ok(Self::Bytes),
            10 => Ok(Self::Array),
            11 => Ok(Self::Object),
            other => {
                Err(ElmdbError::new(ErrorCode::InvalidWireFrame, format!("unknown canonical value tag {other:#04x}")))
            }
        }
    }
}

/// Owned canonical value with strictly sorted, deduplicated `Object` keys and finite-only floats.
#[derive(Debug, Clone)]
pub enum CanonicalValue {
    Null,
    Bool(bool),
    Int(i64),
    /// Always finite (NaN and ±Infinity are rejected at parse boundary); `-0.0` canonicalized to `0.0`.
    Float(f64),
    Timestamp {
        ms: i64,
        ns: u32,
    },
    EntityRef(u64),
    DanglingRef(String),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<CanonicalValue>),
    /// Keys are strictly sorted ascending and deduplicated (O(log N) lookup, O(N) deep equality).
    Object(Vec<(String, CanonicalValue)>),
}

impl CanonicalValue {
    /// Constructs a canonical finite float (normalizing `-0.0` to `+0.0`).
    pub fn float(v: f64) -> Result<Self> {
        if !v.is_finite() {
            return Err(ElmdbError::new(
                ErrorCode::ValidationFailed,
                "non-finite float (NaN or Infinity) is not permitted",
            ));
        }
        Ok(Self::Float(if v == 0.0 { 0.0 } else { v }))
    }

    /// Constructs a canonical `Object`, sorting keys and rejecting duplicates.
    pub fn object(mut entries: Vec<(String, CanonicalValue)>) -> Result<Self> {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for pair in entries.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(ElmdbError::new(
                    ErrorCode::InvalidWireFrame,
                    format!("duplicate object key '{}'", pair[0].0),
                ));
            }
        }
        Ok(Self::Object(entries))
    }

    pub fn object_get(&self, key: &str) -> Option<&CanonicalValue> {
        match self {
            Self::Object(entries) => {
                entries.binary_search_by(|(k, _)| k.as_str().cmp(key)).ok().map(|idx| &entries[idx].1)
            }
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(f) => Some(*f),
            Self::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// Recursively verifies that a Rust-constructed `CanonicalValue` upholds every wire invariant
    /// (finite floats, `ns < 1e9`, strictly sorted unique object keys, nesting depth <= 64).
    pub fn validate_canonical(&self) -> Result<()> {
        self.validate_canonical_inner(0)
    }

    fn validate_canonical_inner(&self, depth: usize) -> Result<()> {
        if depth > MAX_VALUE_DEPTH {
            return Err(ElmdbError::new(ErrorCode::ValidationFailed, "value nesting depth exceeded"));
        }
        match self {
            Self::Float(f) if !f.is_finite() => {
                Err(ElmdbError::new(ErrorCode::ValidationFailed, "non-finite float is not permitted"))
            }
            Self::Timestamp { ns, .. } if *ns >= 1_000_000_000 => {
                Err(ElmdbError::new(ErrorCode::ValidationFailed, "timestamp ns must be < 1e9"))
            }
            Self::Array(items) => {
                for item in items {
                    item.validate_canonical_inner(depth + 1)?;
                }
                Ok(())
            }
            Self::Object(entries) => {
                for (k, _) in entries {
                    if k == "__proto__" {
                        return Err(ElmdbError::new(
                            ErrorCode::ValidationFailed,
                            "reserved key '__proto__' is not permitted",
                        ));
                    }
                }
                for pair in entries.windows(2) {
                    if pair[0].0 >= pair[1].0 {
                        return Err(ElmdbError::new(
                            ErrorCode::ValidationFailed,
                            "object keys must be strictly sorted and unique",
                        ));
                    }
                }
                for (_, v) in entries {
                    v.validate_canonical_inner(depth + 1)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Returns true if this value contains an internal `EntityRef` or `DanglingRef` anywhere inside.
    pub fn contains_internal_ref(&self) -> bool {
        match self {
            Self::EntityRef(_) | Self::DanglingRef(_) => true,
            Self::Array(items) => items.iter().any(Self::contains_internal_ref),
            Self::Object(entries) => entries.iter().any(|(_, v)| v.contains_internal_ref()),
            _ => false,
        }
    }

    /// Returns true if any nested `Object` key equals `"__proto__"`.
    pub fn contains_proto_key(&self) -> bool {
        match self {
            Self::Array(items) => items.iter().any(Self::contains_proto_key),
            Self::Object(entries) => entries.iter().any(|(k, v)| k == "__proto__" || v.contains_proto_key()),
            _ => false,
        }
    }
}

impl PartialEq for CanonicalValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => {
                // Canonicalize `-0.0` to `+0.0`, and compare non-finite floats by canonical NaN/bits
                // so `PartialEq` strictly upholds `Eq` reflexivity even if callers construct raw `Float(NaN)`.
                if a.is_nan() && b.is_nan() {
                    return true;
                }
                let na = if *a == 0.0 { 0.0 } else { *a };
                let nb = if *b == 0.0 { 0.0 } else { *b };
                na.to_bits() == nb.to_bits()
            }
            // Unified numeric equality: Int(5) == Float(5.0), exact past 2^53; non-finite is never equal.
            (Self::Int(i), Self::Float(f)) | (Self::Float(f), Self::Int(i)) => {
                f.is_finite() && compare_int_float(*i, *f) == Ordering::Equal
            }
            (Self::Timestamp { ms: m1, ns: n1 }, Self::Timestamp { ms: m2, ns: n2 }) => m1 == m2 && n1 == n2,
            (Self::EntityRef(a), Self::EntityRef(b)) => a == b,
            (Self::DanglingRef(a), Self::DanglingRef(b)) => a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            (Self::Array(a), Self::Array(b)) => a == b,
            (Self::Object(a), Self::Object(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for CanonicalValue {}

/// Zero-copy borrowed view directly over canonical binary bytes (from LMDB `&'txn [u8]` or wire slice).
#[derive(Debug, Clone, Copy)]
pub enum ValueRef<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Timestamp {
        ms: i64,
        ns: u32,
    },
    EntityRef(u64),
    DanglingRef(&'a str),
    String(&'a str),
    Bytes(&'a [u8]),
    /// Raw canonical bytes of an encoded array payload (elements decoded on demand).
    ArrayRaw(&'a [u8]),
    /// Raw canonical bytes of an encoded object payload (entries decoded on demand).
    ObjectRaw(&'a [u8]),
}

/// Encodes a finite `f64` into an 8-byte unsigned big-endian total order key (`-0.0` normalized to `+0.0`).
#[inline]
fn sortable_f64_bytes(f: f64) -> [u8; 8] {
    let norm = if f == 0.0 { 0.0f64 } else { f };
    let bits = norm.to_bits();
    let ordered = if (bits >> 63) != 0 { !bits } else { bits ^ (1u64 << 63) };
    ordered.to_be_bytes()
}

/// Exact total ordering between `i: i64` and finite `f: f64` without 2^53 precision loss.
pub fn compare_int_float(i: i64, f: f64) -> Ordering {
    debug_assert!(f.is_finite());
    // Out-of-i64-range floats settle immediately.
    if f < (i64::MIN as f64) {
        return Ordering::Greater;
    }
    // `2^63` as f64 is exact (`9223372036854775808.0`), strictly greater than `i64::MAX`.
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    if f >= TWO_POW_63 {
        return Ordering::Less;
    }
    let trunc = f.trunc();
    let f_int = trunc as i64;
    match i.cmp(&f_int) {
        Ordering::Less => Ordering::Less,
        Ordering::Greater => Ordering::Greater,
        Ordering::Equal => {
            let frac = f - trunc;
            if frac > 0.0 {
                Ordering::Less
            } else if frac < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
    }
}

/// Compares any two numeric `CanonicalValue`s (`Int` or `Float`) with exact total ordering.
pub fn compare_numeric(a: &CanonicalValue, b: &CanonicalValue) -> Option<Ordering> {
    match (a, b) {
        (CanonicalValue::Int(x), CanonicalValue::Int(y)) => Some(x.cmp(y)),
        (CanonicalValue::Float(x), CanonicalValue::Float(y)) if x.is_finite() && y.is_finite() => {
            let nx = if *x == 0.0 { 0.0 } else { *x };
            let ny = if *y == 0.0 { 0.0 } else { *y };
            nx.partial_cmp(&ny)
        }
        (CanonicalValue::Int(x), CanonicalValue::Float(y)) if y.is_finite() => Some(compare_int_float(*x, *y)),
        (CanonicalValue::Float(x), CanonicalValue::Int(y)) if x.is_finite() => {
            Some(compare_int_float(*y, *x).reverse())
        }
        _ => None,
    }
}

/// Encodes `Int(i64)` or finite `Float(f64)` into a unified 10-byte lexicographically sortable
/// index key where:
/// - `Int` and `Float` share one single numeric domain (`encode(Int(5)) == encode(Float(5.0))`)
/// - Order is strictly preserved for ALL `i64` values (including `|i| > 2^53`) and ALL finite `f64` values!
pub fn encode_numeric_index_key(val: &CanonicalValue) -> Option<[u8; NUMERIC_INDEX_KEY_LEN]> {
    let mut out = [0u8; NUMERIC_INDEX_KEY_LEN];
    match *val {
        CanonicalValue::Float(f) if f.is_finite() => {
            out[0..8].copy_from_slice(&sortable_f64_bytes(f));
            out[8..10].copy_from_slice(&0x8000u16.to_be_bytes());
            Some(out)
        }
        CanonicalValue::Int(i) => {
            let f = i as f64;
            out[0..8].copy_from_slice(&sortable_f64_bytes(f));
            // For |i| <= 2^53, `f as i128 == i as i128` -> delta == 0 -> biased == 0x8000 (exact match with Float!).
            // For |i| > 2^53, `f` is an exact integer in [-2^63, 2^63] and `|i - f| <= 512`, fitting in i16!
            let f_exact = f as i128;
            let delta = (i128::from(i) - f_exact) as i32;
            debug_assert!((-512..=512).contains(&delta));
            let biased = (delta + 0x8000) as u16;
            out[8..10].copy_from_slice(&biased.to_be_bytes());
            Some(out)
        }
        _ => None,
    }
}

/// Appends the canonical binary encoding of `val` onto `out`.
pub fn encode_value(val: &CanonicalValue, out: &mut Vec<u8>) {
    match val {
        CanonicalValue::Null => out.push(ValueTag::Null as u8),
        CanonicalValue::Bool(false) => out.push(ValueTag::BoolFalse as u8),
        CanonicalValue::Bool(true) => out.push(ValueTag::BoolTrue as u8),
        CanonicalValue::Int(i) => {
            out.push(ValueTag::Int as u8);
            out.extend_from_slice(&i.to_le_bytes());
        }
        CanonicalValue::Float(f) => {
            let norm = if *f == 0.0 { 0.0 } else { *f };
            out.push(ValueTag::Float as u8);
            out.extend_from_slice(&norm.to_bits().to_le_bytes());
        }
        CanonicalValue::Timestamp { ms, ns } => {
            out.push(ValueTag::Timestamp as u8);
            out.extend_from_slice(&ms.to_le_bytes());
            out.extend_from_slice(&ns.to_le_bytes());
        }
        CanonicalValue::EntityRef(key) => {
            out.push(ValueTag::EntityRef as u8);
            out.extend_from_slice(&key.to_le_bytes());
        }
        CanonicalValue::DanglingRef(s) => {
            out.push(ValueTag::DanglingRef as u8);
            write_u32(s.len(), out);
            out.extend_from_slice(s.as_bytes());
        }
        CanonicalValue::String(s) => {
            out.push(ValueTag::String as u8);
            write_u32(s.len(), out);
            out.extend_from_slice(s.as_bytes());
        }
        CanonicalValue::Bytes(b) => {
            out.push(ValueTag::Bytes as u8);
            write_u32(b.len(), out);
            out.extend_from_slice(b);
        }
        CanonicalValue::Array(items) => {
            out.push(ValueTag::Array as u8);
            let len_pos = out.len();
            out.extend_from_slice(&0u32.to_le_bytes()); // total payload byte length (enables O(1) skip)
            write_u32(items.len(), out);
            let payload_start = out.len();
            for item in items {
                encode_value(item, out);
            }
            let payload_len = u32::try_from(out.len() - payload_start + 4).expect("wire array payload fits u32");
            out[len_pos..len_pos + 4].copy_from_slice(&payload_len.to_le_bytes());
        }
        CanonicalValue::Object(entries) => {
            out.push(ValueTag::Object as u8);
            let len_pos = out.len();
            out.extend_from_slice(&0u32.to_le_bytes()); // total payload byte length (enables O(1) skip)
            write_u32(entries.len(), out);
            let payload_start = out.len();
            for (k, v) in entries {
                write_u32(k.len(), out);
                out.extend_from_slice(k.as_bytes());
                encode_value(v, out);
            }
            let payload_len = u32::try_from(out.len() - payload_start + 4).expect("wire object payload fits u32");
            out[len_pos..len_pos + 4].copy_from_slice(&payload_len.to_le_bytes());
        }
    }
}

fn write_u32(len: usize, out: &mut Vec<u8>) {
    let n = u32::try_from(len).expect("wire item length fits u32");
    out.extend_from_slice(&n.to_le_bytes());
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidWireFrame, "wire offset overflow"))?;
        if end > self.buf.len() {
            return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "unexpected EOF in canonical value"));
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn read_i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn read_str(&mut self) -> Result<&'a str> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map_err(|_| ElmdbError::new(ErrorCode::InvalidWireFrame, "invalid UTF-8 in canonical value"))
    }
}

/// Decodes one `CanonicalValue` from `buf`, returning `(value, bytes_consumed)`.
pub fn decode_value(buf: &[u8]) -> Result<(CanonicalValue, usize)> {
    let mut r = Reader::new(buf);
    let val = decode_inner(&mut r, 0)?;
    Ok((val, r.pos))
}

fn decode_inner(r: &mut Reader<'_>, depth: usize) -> Result<CanonicalValue> {
    if depth > MAX_VALUE_DEPTH {
        return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "value nesting depth exceeded"));
    }
    match ValueTag::from_u8(r.read_u8()?)? {
        ValueTag::Null => Ok(CanonicalValue::Null),
        ValueTag::BoolFalse => Ok(CanonicalValue::Bool(false)),
        ValueTag::BoolTrue => Ok(CanonicalValue::Bool(true)),
        ValueTag::Int => Ok(CanonicalValue::Int(r.read_i64()?)),
        ValueTag::Float => {
            let f = f64::from_bits(r.read_u64()?);
            if !f.is_finite() {
                return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "non-finite float"));
            }
            Ok(CanonicalValue::Float(if f == 0.0 { 0.0 } else { f }))
        }
        ValueTag::Timestamp => {
            let ms = r.read_i64()?;
            let ns = r.read_u32()?;
            if ns >= 1_000_000_000 {
                return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "timestamp ns >= 1e9"));
            }
            Ok(CanonicalValue::Timestamp { ms, ns })
        }
        ValueTag::EntityRef => Ok(CanonicalValue::EntityRef(r.read_u64()?)),
        ValueTag::DanglingRef => Ok(CanonicalValue::DanglingRef(r.read_str()?.to_owned())),
        ValueTag::String => Ok(CanonicalValue::String(r.read_str()?.to_owned())),
        ValueTag::Bytes => {
            let len = r.read_u32()? as usize;
            Ok(CanonicalValue::Bytes(r.take(len)?.to_vec()))
        }
        ValueTag::Array => {
            let byte_len = r.read_u32()? as usize;
            let payload = r.take(byte_len)?;
            let mut sub = Reader::new(payload);
            let count = sub.read_u32()? as usize;
            if count > byte_len {
                return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "corrupt array count"));
            }
            let mut items = Vec::with_capacity(count.min(256));
            for _ in 0..count {
                items.push(decode_inner(&mut sub, depth + 1)?);
            }
            if sub.pos != payload.len() {
                return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "trailing bytes in array"));
            }
            Ok(CanonicalValue::Array(items))
        }
        ValueTag::Object => {
            let byte_len = r.read_u32()? as usize;
            let payload = r.take(byte_len)?;
            let mut sub = Reader::new(payload);
            let count = sub.read_u32()? as usize;
            if count > byte_len / 5 + 1 {
                return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "corrupt object count"));
            }
            let mut entries: Vec<(String, CanonicalValue)> = Vec::with_capacity(count.min(256));
            for _ in 0..count {
                let k = sub.read_str()?.to_owned();
                if let Some((prev, _)) = entries.last()
                    && prev.as_str() >= k.as_str()
                {
                    return Err(ElmdbError::new(
                        ErrorCode::InvalidWireFrame,
                        "object keys not strictly sorted and unique",
                    ));
                }
                let v = decode_inner(&mut sub, depth + 1)?;
                entries.push((k, v));
            }
            if sub.pos != payload.len() {
                return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "trailing bytes in object"));
            }
            Ok(CanonicalValue::Object(entries))
        }
    }
}

/// Zero-copy decode of a single canonical value from `buf` (zero heap allocations!).
pub fn decode_value_ref(buf: &[u8]) -> Result<(ValueRef<'_>, usize)> {
    let mut r = Reader::new(buf);
    let val = match ValueTag::from_u8(r.read_u8()?)? {
        ValueTag::Null => ValueRef::Null,
        ValueTag::BoolFalse => ValueRef::Bool(false),
        ValueTag::BoolTrue => ValueRef::Bool(true),
        ValueTag::Int => ValueRef::Int(r.read_i64()?),
        ValueTag::Float => {
            let f = f64::from_bits(r.read_u64()?);
            if !f.is_finite() {
                return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "non-finite float"));
            }
            ValueRef::Float(if f == 0.0 { 0.0 } else { f })
        }
        ValueTag::Timestamp => {
            let ms = r.read_i64()?;
            let ns = r.read_u32()?;
            if ns >= 1_000_000_000 {
                return Err(ElmdbError::new(ErrorCode::InvalidWireFrame, "timestamp ns >= 1e9"));
            }
            ValueRef::Timestamp { ms, ns }
        }
        ValueTag::EntityRef => ValueRef::EntityRef(r.read_u64()?),
        ValueTag::DanglingRef => ValueRef::DanglingRef(r.read_str()?),
        ValueTag::String => ValueRef::String(r.read_str()?),
        ValueTag::Bytes => {
            let len = r.read_u32()? as usize;
            ValueRef::Bytes(r.take(len)?)
        }
        ValueTag::Array => {
            let byte_len = r.read_u32()? as usize;
            ValueRef::ArrayRaw(r.take(byte_len)?)
        }
        ValueTag::Object => {
            let byte_len = r.read_u32()? as usize;
            ValueRef::ObjectRaw(r.take(byte_len)?)
        }
    };
    Ok((val, r.pos))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn unified_numeric_key_exact_corner_cases() {
        // Int(5) == Float(5.0), -0.0 == +0.0 == Int(0)
        assert_eq!(
            encode_numeric_index_key(&CanonicalValue::Int(5)),
            encode_numeric_index_key(&CanonicalValue::Float(5.0))
        );
        assert_eq!(
            encode_numeric_index_key(&CanonicalValue::Int(0)),
            encode_numeric_index_key(&CanonicalValue::Float(-0.0))
        );

        // Values past 2^53 preserve exact ordering even when `i as f64` collides!
        let p53 = 1i64 << 53;
        let samples = [
            CanonicalValue::Float(f64::MIN),
            CanonicalValue::Int(i64::MIN),
            CanonicalValue::Int(i64::MIN + 1),
            CanonicalValue::Int(-p53 - 2),
            CanonicalValue::Int(-p53 - 1),
            CanonicalValue::Int(-p53),
            CanonicalValue::Float(-0.5),
            CanonicalValue::Int(0),
            CanonicalValue::Float(0.5),
            CanonicalValue::Int(p53),
            CanonicalValue::Int(p53 + 1),
            CanonicalValue::Int(p53 + 2),
            CanonicalValue::Int(i64::MAX - 2),
            CanonicalValue::Int(i64::MAX - 1),
            CanonicalValue::Int(i64::MAX),
            CanonicalValue::Float(9_223_372_036_854_775_808.0), // 2^63 exact
            CanonicalValue::Float(f64::MAX),
        ];
        for pair in samples.windows(2) {
            let k0 = encode_numeric_index_key(&pair[0]).unwrap();
            let k1 = encode_numeric_index_key(&pair[1]).unwrap();
            assert!(k0 < k1, "expected {:?} < {:?}", pair[0], pair[1]);
            assert_eq!(compare_numeric(&pair[0], &pair[1]), Some(Ordering::Less));
        }
    }

    proptest! {
        #[test]
        fn unified_numeric_key_matches_exact_compare_for_all_ints_and_floats(
            a_is_int in any::<bool>(),
            a_i in any::<i64>(),
            a_f in any::<f64>().prop_filter("finite", |f| f.is_finite()),
            b_is_int in any::<bool>(),
            b_i in any::<i64>(),
            b_f in any::<f64>().prop_filter("finite", |f| f.is_finite()),
        ) {
            let a = if a_is_int { CanonicalValue::Int(a_i) } else { CanonicalValue::float(a_f).unwrap() };
            let b = if b_is_int { CanonicalValue::Int(b_i) } else { CanonicalValue::float(b_f).unwrap() };
            let ka = encode_numeric_index_key(&a).unwrap();
            let kb = encode_numeric_index_key(&b).unwrap();
            prop_assert_eq!(ka.cmp(&kb), compare_numeric(&a, &b).unwrap());
            prop_assert_eq!(ka == kb, a == b);
        }
    }

    #[test]
    fn canonical_value_roundtrips_owned_and_zero_copy() {
        let obj = CanonicalValue::object(vec![
            ("z".into(), CanonicalValue::Int(i64::MAX)),
            ("a".into(), CanonicalValue::String("hello".into())),
            ("ref".into(), CanonicalValue::EntityRef(99)),
            ("dangling".into(), CanonicalValue::DanglingRef("Task-old".into())),
            ("ts".into(), CanonicalValue::Timestamp { ms: 1_700_000_000_000, ns: 123_456 }),
            ("arr".into(), CanonicalValue::Array(vec![CanonicalValue::Bool(true), CanonicalValue::Null])),
        ])
        .unwrap();

        let mut buf = Vec::new();
        encode_value(&obj, &mut buf);
        let (decoded, consumed) = decode_value(&buf).unwrap();
        assert_eq!(consumed, buf.len());
        assert_eq!(decoded, obj);

        let (vref, ref_consumed) = decode_value_ref(&buf).unwrap();
        assert_eq!(ref_consumed, buf.len());
        assert!(matches!(vref, ValueRef::ObjectRaw(_)));
    }
}
