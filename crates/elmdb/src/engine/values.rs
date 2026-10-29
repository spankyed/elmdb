//! Values as the engine handles them: encoded and decoded, checked against a field's declared type, compared, and
//! filed in an index.

use rmpv::Value;

use crate::error::{Error, ErrorCode, Result};
use crate::format::key::Part;
use crate::format::value::{MAX_DEPTH, TIMESTAMP_EXTENSION};
use crate::schema::{FieldDef, ValueType};

/// The longest string, in UTF-8 bytes, an index holds; a longer one is found by walking the type's rows. A key is
/// at most 511 bytes (LMDB's limit), which this leaves room in for the type, the index's name and the sequence
pub const MAX_INDEXED_STRING: usize = 256;

/// The longest key LMDB stores, in bytes: a write whose key would be longer is refused rather than failing in LMDB
pub const MAX_KEY_BYTES: usize = 511;

pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, value).expect("writing to a Vec doesn't fail");
    out
}

/// The byte a compressed field value starts with as it is stored: one no msgpack value starts with, so a stored value
/// is its msgpack, or this byte and a zstd frame of its msgpack (`docs/architecture/format.md`, Values)
pub const COMPRESSED: u8 = 0xc1;

/// The msgpack length from which a field's value is stored compressed, when compressing makes it smaller: set by the
/// sizes and reads measured on production-shaped data (`docs/evidence/next-phase-8.md`)
pub const COMPRESS_FROM: usize = 256;

/// zstd's level: within a few percent of its smallest output on apack's text, and its fastest to encode and decode
const ZSTD_LEVEL: i32 = 3;

/// A field's value as it is stored: its msgpack, compressed from `COMPRESS_FROM` bytes when that is smaller
pub fn stored(value: &Value) -> Vec<u8> {
    let plain = encode(value);
    if plain.len() < COMPRESS_FROM {
        return plain;
    }
    match zstd::bulk::compress(&plain, ZSTD_LEVEL) {
        Ok(frame) if frame.len() + 1 < plain.len() => {
            let mut out = Vec::with_capacity(frame.len() + 1);
            out.push(COMPRESSED);
            out.extend_from_slice(&frame);
            out
        }
        _ => plain,
    }
}

/// A stored field value's msgpack: the bytes as they are, or decompressed
pub fn unstored(bytes: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    match bytes.split_first() {
        Some((&COMPRESSED, frame)) => decompress(frame)
            .map(std::borrow::Cow::Owned)
            .map_err(|e| Error::new(ErrorCode::Corrupted, format!("A stored value doesn't decompress: {e}"))),
        _ => Ok(std::borrow::Cow::Borrowed(bytes)),
    }
}

/// `unstored` of bytes the caller owns: a value stored uncompressed is those bytes, not a copy of them
pub fn unstored_owned(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.first() == Some(&COMPRESSED) { Ok(unstored(&bytes)?.into_owned()) } else { Ok(bytes) }
}

/// The largest value decompressed in one call into a buffer of the size its frame records. A frame that records more,
/// or no size, is read as a stream, so a damaged header can't ask for a large allocation
const ONE_CALL_UP_TO: u64 = 16 << 20;

thread_local! {
    /// One decompression context a thread, for every value it reads: a context costs about 100 KB to allocate, where
    /// the values it reads are a kilobyte or two (`docs/evidence/read-allocation-codec.md`). Each call decompresses one
    /// whole frame, so nothing one value leaves in it reaches the next
    static DECOMPRESSOR: std::cell::RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { std::cell::RefCell::new(None) };
}

fn decompress(frame: &[u8]) -> std::io::Result<Vec<u8>> {
    let size = match zstd::zstd_safe::get_frame_content_size(frame) {
        Ok(Some(size)) if size <= ONE_CALL_UP_TO => size as usize,
        _ => return zstd::stream::decode_all(frame),
    };
    DECOMPRESSOR.with(|context| {
        let mut context = context.borrow_mut();
        let decompressor = match context.as_mut() {
            Some(decompressor) => decompressor,
            None => context.insert(zstd::bulk::Decompressor::new()?),
        };
        let mut out = Vec::with_capacity(size);
        decompressor.decompress_to_buffer(frame, &mut out)?;
        Ok(out)
    })
}

/// A stored field value, decoded
pub fn decode_stored(bytes: &[u8]) -> Result<Value> {
    decode(&unstored(bytes)?)
}

/// A stored value, decoded; stored values were checked as they were written
pub fn decode(bytes: &[u8]) -> Result<Value> {
    rmpv::decode::read_value_with_max_depth(&mut &bytes[..], MAX_DEPTH)
        .map_err(|e| Error::new(ErrorCode::Corrupted, format!("A stored value doesn't decode: {e}")))
}

/// Checks a value a write brings is in the value spec (`format::value`), and nests no deeper than it allows
pub fn check_spec(value: &Value) -> Result<()> {
    crate::format::value::decode(&encode(value)).map(|_| ())
}

pub fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(i) => i.as_f64(),
        Value::F32(n) => Some(f64::from(*n)),
        Value::F64(n) => Some(*n),
        _ => None,
    }
}

/// Whether two values are the same value, as apack's `sameValue`: equal scalars (an integer and a float of the same
/// number are equal), timestamps at the same time, lists equal element by element, and maps with equal fields in any
/// order
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q)),
        (Value::Map(x), Value::Map(y)) => {
            if x.len() != y.len() {
                return false;
            }
            // A map may preserve repeated keys. Match each entry once so equality preserves multiplicity;
            // reusing one entry for several matches makes equality asymmetric and breaks unique constraints.
            let mut matched = vec![false; y.len()];
            for (key, value) in x {
                let Some(position) = y.iter().enumerate().find_map(|(i, (other_key, other_value))| {
                    (!matched[i] && key == other_key && same(value, other_value)).then_some(i)
                }) else {
                    return false;
                };
                matched[position] = true;
            }
            true
        }
        _ => scalar_equal(a, b),
    }
}

/// JavaScript's `===` over decoded values: scalars by value, and no list or map equal to anything
pub fn strict_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(_) | Value::Map(_), _) | (_, Value::Array(_) | Value::Map(_)) => false,
        _ => scalar_equal(a, b),
    }
}

fn scalar_equal(a: &Value, b: &Value) -> bool {
    match (as_number(a), as_number(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// Whether a stored value is `probe`, or a list holding it: what an equality filter matches
pub fn holds(stored: &Value, probe: &Value) -> bool {
    strict_equal(stored, probe) || matches!(stored, Value::Array(items) if items.iter().any(|v| strict_equal(v, probe)))
}

/// The key part an index files a scalar under, or `None` for a value no index holds (a map, a list, bytes, a
/// timestamp, a string of more than `MAX_INDEXED_STRING` bytes)
pub fn index_part(value: &Value) -> Option<Part> {
    match value {
        Value::Nil => Some(Part::Null),
        Value::Boolean(b) => Some(Part::Bool(*b)),
        Value::String(s) => {
            let s = s.as_str()?;
            (s.len() <= MAX_INDEXED_STRING).then(|| Part::String(s.to_owned()))
        }
        other => as_number(other).filter(|n| n.is_finite()).map(Part::Number),
    }
}

/// The parts a single-field index files a value under: the value, or each distinct element of a list
pub fn index_parts(value: Option<&Value>) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    let mut push = |v: &Value| {
        if let Some(p) = index_part(v)
            && !parts.contains(&p)
        {
            parts.push(p);
        }
    };
    match value {
        None => {}
        Some(Value::Array(items)) => items.iter().for_each(&mut push),
        Some(v) => push(v),
    }
    parts
}

/// Refuses a value its field doesn't take (`FIELD_TYPE`)
pub fn check_field(entity_type: &str, field: &str, def: &FieldDef, value: &Value) -> Result<()> {
    if matches!(value, Value::Nil) {
        if def.nullable || def.value == ValueType::Any {
            return Ok(());
        }
        return Err(Error::new(
            ErrorCode::FieldType,
            format!("{entity_type}.{field} isn't nullable, so it can't hold null"),
        ));
    }
    match takes(&def.value, value) {
        Ok(()) => Ok(()),
        Err(why) => {
            Err(Error::new(ErrorCode::FieldType, format!("{entity_type}.{field} is {}: {why}", describe(&def.value))))
        }
    }
}

fn describe(t: &ValueType) -> String {
    match t {
        ValueType::Enum { values } => {
            format!("one of {}", values.iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>().join(", "))
        }
        ValueType::Ref { to } => format!("an id of {}", to.join(" or ")),
        ValueType::List { of } => format!("a list of {}", describe(of)),
        other => format!("a {}", other.name()),
    }
}

/// Whether a type takes a value, and why not
fn takes(t: &ValueType, v: &Value) -> std::result::Result<(), String> {
    let kind = || kind_of(v);
    let ok = match t {
        ValueType::Any => true,
        ValueType::String => v.is_str(),
        ValueType::Number => as_number(v).is_some_and(f64::is_finite),
        ValueType::Integer => as_number(v).is_some_and(|n| n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_992.0),
        ValueType::Boolean => v.is_bool(),
        ValueType::Bytes => matches!(v, Value::Binary(_)),
        ValueType::Object => v.is_map(),
        ValueType::Enum { values } => {
            if let Some(s) = v.as_str() {
                if values.iter().any(|x| x == s) {
                    return Ok(());
                }
                return Err(format!("\"{s}\" isn't one"));
            }
            false
        }
        ValueType::Ref { to } => {
            if let Some(s) = v.as_str() {
                let t = super::ids::type_of(s);
                if to.iter().any(|x| x == t) {
                    return Ok(());
                }
                return Err(format!("\"{s}\" is an id of {t}"));
            }
            false
        }
        ValueType::List { of } => {
            let Some(items) = v.as_array() else { return Err(format!("it got {}", kind())) };
            for (i, item) in items.iter().enumerate() {
                takes(of, item).map_err(|why| format!("element {i}: {why}"))?;
            }
            return Ok(());
        }
    };
    if ok { Ok(()) } else { Err(format!("it got {}", kind())) }
}

pub fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Nil => "null",
        Value::Boolean(_) => "a boolean",
        Value::Integer(_) | Value::F32(_) | Value::F64(_) => "a number",
        Value::String(_) => "a string",
        Value::Binary(_) => "bytes",
        Value::Array(_) => "a list",
        Value::Map(_) => "a map",
        Value::Ext(t, _) if *t == TIMESTAMP_EXTENSION => "a timestamp",
        Value::Ext(..) => "an extension",
    }
}

/// `value` merged into `current`: the two maps' fields, `value`'s winning, or `value` when either isn't a map
pub fn merged(current: Option<&Value>, value: &Value) -> Value {
    match (current, value) {
        (Some(Value::Map(cur)), Value::Map(new)) => {
            let mut out = cur.clone();
            for (k, v) in new {
                match out.iter_mut().find(|(k2, _)| k2 == k) {
                    Some(slot) => slot.1 = v.clone(),
                    None => out.push((k.clone(), v.clone())),
                }
            }
            Value::Map(out)
        }
        _ => value.clone(),
    }
}

/// Whether a list element matches what `removeValue` names: itself, or, for a map, an element holding each of its fields
pub fn matches_removal(element: &Value, criterion: &Value) -> bool {
    if strict_equal(element, criterion) {
        return true;
    }
    match (element, criterion) {
        (Value::Map(e), Value::Map(c)) => {
            c.iter().all(|(k, v)| e.iter().any(|(k2, v2)| k2 == k && strict_equal(v2, v)))
        }
        _ => false,
    }
}

/// A msgpack timestamp's milliseconds: 32 bits of seconds, 64 bits of nanoseconds and seconds, or 96 bits
pub fn timestamp_ms(data: &[u8]) -> f64 {
    match data.len() {
        4 => f64::from(u32::from_be_bytes(data.try_into().unwrap())) * 1000.0,
        8 => {
            let v = u64::from_be_bytes(data.try_into().unwrap());
            (v & 0x3_ffff_ffff) as f64 * 1000.0 + (v >> 34) as f64 / 1e6
        }
        12 => {
            let nanos = u32::from_be_bytes(data[..4].try_into().unwrap());
            let secs = i64::from_be_bytes(data[4..].try_into().unwrap());
            secs as f64 * 1000.0 + f64::from(nanos) / 1e6
        }
        _ => 0.0,
    }
}

/// A time a value holds, in milliseconds since the epoch: a number, or a timestamp
pub fn as_time(value: &Value) -> Option<f64> {
    match value {
        Value::Ext(t, data) if *t == TIMESTAMP_EXTENSION => Some(timestamp_ms(data)),
        other => as_number(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::FieldDef;

    fn field(value: ValueType) -> FieldDef {
        FieldDef { value, optional: false, nullable: false }
    }

    #[test]
    fn checks_values_against_their_field() {
        let ok = |t: ValueType, v: Value| check_field("Note", "f", &field(t), &v);
        assert!(ok(ValueType::String, "a".into()).is_ok());
        assert!(ok(ValueType::Integer, Value::from(3.0)).is_ok(), "a float holding a whole number is an integer");
        assert!(ok(ValueType::Integer, Value::from(3.5)).is_err());
        assert!(
            ok(ValueType::Enum { values: vec!["a".into()] }, "b".into())
                .unwrap_err()
                .message
                .contains("\"b\" isn't one")
        );
        assert!(
            ok(ValueType::Ref { to: vec!["Note".into()] }, "Task-1".into())
                .unwrap_err()
                .message
                .contains("an id of Task")
        );
        let list = ValueType::List { of: Box::new(ValueType::Number) };
        assert!(ok(list.clone(), Value::Array(vec![1.into(), "x".into()])).unwrap_err().message.contains("element 1"));
        assert!(ok(ValueType::String, Value::Nil).unwrap_err().message.contains("isn't nullable"));
        assert!(
            check_field("Note", "f", &FieldDef { nullable: true, ..field(ValueType::String) }, &Value::Nil).is_ok()
        );
        assert!(ok(ValueType::Any, Value::Nil).is_ok());
        let got = |t: ValueType, v: Value| ok(t, v).unwrap_err().message;
        assert!(got(ValueType::String, 1.into()).contains("it got a number"));
        assert!(got(ValueType::String, Value::Ext(TIMESTAMP_EXTENSION, vec![0; 4])).contains("it got a timestamp"));
        assert!(got(ValueType::String, Value::Ext(5, vec![0])).contains("it got an extension"));
        assert!(got(list, "x".into()).contains("it got a string"));
    }

    #[test]
    fn reads_each_timestamp_encoding_as_milliseconds() {
        let secs: u32 = 1_700_000_000;
        assert_eq!(timestamp_ms(&secs.to_be_bytes()), 1_700_000_000_000.0);
        let packed = (500_000_000_u64 << 34) | u64::from(secs);
        assert_eq!(timestamp_ms(&packed.to_be_bytes()), 1_700_000_000_500.0);
        let wide = [250_000_000_u32.to_be_bytes().as_slice(), (-2_i64).to_be_bytes().as_slice()].concat();
        assert_eq!(timestamp_ms(&wide), -1_750.0);
        let ts = Value::Ext(TIMESTAMP_EXTENSION, secs.to_be_bytes().to_vec());
        assert_eq!(as_time(&ts), Some(1_700_000_000_000.0));
        assert_eq!(as_time(&Value::Ext(5, secs.to_be_bytes().to_vec())), None, "another extension holds no time");
        assert_eq!(as_time(&Value::from(12)), Some(12.0));
        assert_eq!(as_number(&Value::F32(1.5)), Some(1.5));
    }

    #[test]
    fn refuses_a_value_outside_the_value_spec() {
        assert!(check_spec(&Value::Map(vec![("k".into(), 1.into())])).is_ok());
        assert!(check_spec(&Value::Map(vec![(1.into(), 1.into())])).is_err(), "a map key is a string");
    }

    #[test]
    fn compresses_from_the_threshold_on() {
        // A string's msgpack is two bytes longer than it, for these lengths
        let below = Value::from("a".repeat(COMPRESS_FROM - 3));
        assert_eq!(stored(&below), encode(&below));
        let at = Value::from("a".repeat(COMPRESS_FROM - 2));
        assert_eq!(encode(&at).len(), COMPRESS_FROM);
        assert_eq!(stored(&at)[0], COMPRESSED);
    }

    #[test]
    fn compares_as_javascript_does() {
        assert!(strict_equal(&Value::from(1), &Value::from(1.0)));
        assert!(!strict_equal(&Value::Array(vec![]), &Value::Array(vec![])));
        assert!(same(&Value::Array(vec![1.into()]), &Value::Array(vec![1.0.into()])));
        let m = |pairs: Vec<(&str, Value)>| Value::Map(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect());
        assert!(same(&m(vec![("a", 1.into()), ("b", 2.into())]), &m(vec![("b", 2.into()), ("a", 1.into())])));
        let repeated = m(vec![("a", 1.into()), ("a", 1.into())]);
        let distinct = m(vec![("a", 1.into()), ("b", 2.into())]);
        assert!(!same(&repeated, &distinct));
        assert!(!same(&distinct, &repeated));
        assert!(!same(&repeated, &m(vec![("a", 1.into()), ("a", 2.into())])));
        assert!(same(&m(vec![("a", 1.into()), ("a", 2.into())]), &m(vec![("a", 2.0.into()), ("a", 1.0.into())])));
        assert!(!same(&Value::Array(vec![repeated]), &Value::Array(vec![distinct])));
        assert!(matches_removal(&m(vec![("a", 1.into()), ("b", 2.into())]), &m(vec![("a", 1.into())])));
    }

    #[test]
    fn files_scalars_and_each_list_element_once() {
        assert_eq!(
            index_parts(Some(&Value::Array(vec![1.into(), 1.0.into(), "a".into()]))),
            vec![Part::Number(1.0), Part::String("a".into())]
        );
        assert_eq!(index_parts(Some(&Value::from("x".repeat(257)))), vec![]);
        assert_eq!(index_part(&Value::Map(vec![])), None);
    }
}
