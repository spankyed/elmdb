//! The value spec: what a stored value may be (`docs/format.md`, Values). A value is msgpack bytes, stored as the caller
//! wrote them; the engine checks them once, as a write brings them, and decodes them only where it must.

use crate::error::{Error, ErrorCode, Result};

/// msgpack's timestamp extension, how a JavaScript `Date` encodes: the one extension a value may hold
pub const TIMESTAMP_EXTENSION: i8 = -1;

/// The deepest a value nests: arrays and maps inside each other. Deeper is refused rather than walked
pub const MAX_DEPTH: usize = 256;

/// Checks `bytes` are one value in the spec, with nothing after it, and decodes it
pub fn decode(bytes: &[u8]) -> Result<rmpv::Value> {
    let invalid =
        |why: &str| Error::new(ErrorCode::ValueInvalid, format!("A value isn't valid msgpack for elmdb: {why}"));
    // The markers first: rmpv reads the reserved marker as nil, and a value must never hold it
    let mut slice = bytes;
    walk(&mut slice, 0).map_err(|why| invalid(&why))?;
    if !slice.is_empty() {
        return Err(invalid(&format!("{} bytes follow the value", slice.len())));
    }
    let mut slice = bytes;
    let value = rmpv::decode::read_value_with_max_depth(&mut slice, MAX_DEPTH).map_err(|e| invalid(&e.to_string()))?;
    check(&value).map_err(|why| invalid(&why))?;
    Ok(value)
}

/// Steps over one value's bytes, marker by marker, refusing a marker msgpack reserves
fn walk(bytes: &mut &[u8], depth: usize) -> std::result::Result<(), String> {
    use rmp::Marker;
    if depth > MAX_DEPTH {
        return Err(format!("it nests deeper than {MAX_DEPTH}"));
    }
    let marker = rmp::decode::read_marker(bytes).map_err(|_| "it ends early".to_owned())?;
    let skip = |n: usize, bytes: &mut &[u8]| -> std::result::Result<(), String> {
        if bytes.len() < n {
            return Err("it ends early".to_owned());
        }
        *bytes = &bytes[n..];
        Ok(())
    };
    let length = |width: usize, bytes: &mut &[u8]| -> std::result::Result<usize, String> {
        let raw = bytes.get(..width).ok_or("it ends early")?;
        let n = raw.iter().fold(0usize, |n, b| (n << 8) | usize::from(*b));
        *bytes = &bytes[width..];
        Ok(n)
    };
    match marker {
        Marker::Reserved => Err("it holds the reserved marker 0xc1".to_owned()),
        Marker::Null | Marker::True | Marker::False | Marker::FixPos(_) | Marker::FixNeg(_) => Ok(()),
        Marker::U8 | Marker::I8 => skip(1, bytes),
        Marker::U16 | Marker::I16 => skip(2, bytes),
        Marker::U32 | Marker::I32 | Marker::F32 => skip(4, bytes),
        Marker::U64 | Marker::I64 | Marker::F64 => skip(8, bytes),
        Marker::FixStr(n) => skip(usize::from(n), bytes),
        Marker::Str8 | Marker::Bin8 => {
            let n = length(1, bytes)?;
            skip(n, bytes)
        }
        Marker::Str16 | Marker::Bin16 => {
            let n = length(2, bytes)?;
            skip(n, bytes)
        }
        Marker::Str32 | Marker::Bin32 => {
            let n = length(4, bytes)?;
            skip(n, bytes)
        }
        Marker::FixArray(n) => (0..n).try_for_each(|_| walk(bytes, depth + 1)),
        Marker::Array16 => {
            let n = length(2, bytes)?;
            (0..n).try_for_each(|_| walk(bytes, depth + 1))
        }
        Marker::Array32 => {
            let n = length(4, bytes)?;
            (0..n).try_for_each(|_| walk(bytes, depth + 1))
        }
        Marker::FixMap(n) => (0..usize::from(n) * 2).try_for_each(|_| walk(bytes, depth + 1)),
        Marker::Map16 => {
            let n = length(2, bytes)?;
            (0..n * 2).try_for_each(|_| walk(bytes, depth + 1))
        }
        Marker::Map32 => {
            let n = length(4, bytes)?;
            (0..n * 2).try_for_each(|_| walk(bytes, depth + 1))
        }
        Marker::FixExt1 => skip(2, bytes),
        Marker::FixExt2 => skip(3, bytes),
        Marker::FixExt4 => skip(5, bytes),
        Marker::FixExt8 => skip(9, bytes),
        Marker::FixExt16 => skip(17, bytes),
        Marker::Ext8 => {
            let n = length(1, bytes)?;
            skip(n + 1, bytes)
        }
        Marker::Ext16 => {
            let n = length(2, bytes)?;
            skip(n + 1, bytes)
        }
        Marker::Ext32 => {
            let n = length(4, bytes)?;
            skip(n + 1, bytes)
        }
    }
}

fn check(value: &rmpv::Value) -> std::result::Result<(), String> {
    match value {
        rmpv::Value::Array(items) => items.iter().try_for_each(check),
        rmpv::Value::Map(entries) => entries.iter().try_for_each(|(k, v)| {
            if !k.is_str() {
                return Err(format!("a map key is {}, and keys are strings", kind_of(k)));
            }
            check(v)
        }),
        rmpv::Value::Ext(t, _) if *t != TIMESTAMP_EXTENSION => {
            Err(format!("it holds extension type {t}, and the only one allowed is the timestamp (-1)"))
        }
        rmpv::Value::F64(n) if !n.is_finite() => Err("it holds a number that isn't finite".to_owned()),
        rmpv::Value::F32(n) if !n.is_finite() => Err("it holds a number that isn't finite".to_owned()),
        _ => Ok(()),
    }
}

fn kind_of(value: &rmpv::Value) -> &'static str {
    match value {
        rmpv::Value::Nil => "nil",
        rmpv::Value::Boolean(_) => "a boolean",
        rmpv::Value::Integer(_) | rmpv::Value::F32(_) | rmpv::Value::F64(_) => "a number",
        rmpv::Value::String(_) => "a string",
        rmpv::Value::Binary(_) => "binary",
        rmpv::Value::Array(_) => "an array",
        rmpv::Value::Map(_) => "a map",
        rmpv::Value::Ext(..) => "an extension",
    }
}

#[cfg(test)]
mod tests {
    use rmpv::Value;

    use super::*;

    fn bytes(value: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        rmpv::encode::write_value(&mut out, value).unwrap();
        out
    }

    #[test]
    fn takes_what_apack_stores_and_a_date() {
        let value = Value::Map(vec![
            ("text".into(), "hi".into()),
            ("n".into(), Value::from(1.5)),
            ("list".into(), Value::Array(vec![Value::Nil, true.into(), 3.into()])),
            ("at".into(), Value::Ext(TIMESTAMP_EXTENSION, vec![0, 0, 0, 1])),
        ]);
        assert_eq!(decode(&bytes(&value)).unwrap(), value);
    }

    #[test]
    fn refuses_what_the_spec_leaves_out() {
        let refused = |value: Value| decode(&bytes(&value)).unwrap_err();
        let e = refused(Value::Ext(1, vec![1]));
        assert_eq!(e.code, ErrorCode::ValueInvalid);
        assert!(e.message.contains("extension type 1"), "{}", e.message);
        assert!(refused(Value::Map(vec![(1.into(), Value::Nil)])).message.contains("a map key is a number"));
        assert!(refused(Value::Array(vec![Value::from(f64::NAN)])).message.contains("isn't finite"));
        let mut trailing = bytes(&Value::Nil);
        trailing.push(0xc0);
        assert!(decode(&trailing).unwrap_err().message.contains("1 bytes follow"));
        assert_eq!(decode(&[0xc1]).unwrap_err().code, ErrorCode::ValueInvalid);
    }
}
