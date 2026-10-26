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
            x.len() == y.len() && x.iter().all(|(k, v)| y.iter().any(|(k2, v2)| k == k2 && same(v, v2)))
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
    }

    #[test]
    fn compares_as_javascript_does() {
        assert!(strict_equal(&Value::from(1), &Value::from(1.0)));
        assert!(!strict_equal(&Value::Array(vec![]), &Value::Array(vec![])));
        assert!(same(&Value::Array(vec![1.into()]), &Value::Array(vec![1.0.into()])));
        let m = |pairs: Vec<(&str, Value)>| Value::Map(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect());
        assert!(same(&m(vec![("a", 1.into()), ("b", 2.into())]), &m(vec![("b", 2.into()), ("a", 1.into())])));
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
