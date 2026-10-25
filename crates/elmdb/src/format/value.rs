//! The value spec: what a stored value may be (`docs/architecture/format.md`, Values). A value is msgpack bytes, stored as the caller
//! wrote them; the engine checks them once, as a write brings them, and decodes them only where it must.

use crate::error::{Error, ErrorCode, Result};

/// msgpack's timestamp extension, how a JavaScript `Date` encodes: the one extension a value may hold
pub const TIMESTAMP_EXTENSION: i8 = -1;

/// The deepest a value nests: arrays and maps inside each other. Deeper is refused rather than walked
pub const MAX_DEPTH: usize = 256;

fn invalid(why: &str) -> Error {
    Error::new(ErrorCode::ValueInvalid, format!("A value isn't valid msgpack for elmdb: {why}"))
}

/// Checks `bytes` are one value in the spec, with nothing after it, reading them once and allocating nothing
pub fn validate(bytes: &[u8]) -> Result<()> {
    let mut slice = bytes;
    walk(&mut slice, 0).map_err(|why| invalid(&why))?;
    if !slice.is_empty() {
        return Err(invalid(&format!("{} bytes follow the value", slice.len())));
    }
    Ok(())
}

/// Checks `bytes` are one value in the spec, with nothing after it, and decodes it
pub fn decode(bytes: &[u8]) -> Result<rmpv::Value> {
    // The markers first: rmpv reads the reserved marker as nil, and a value must never hold it
    validate(bytes)?;
    let mut slice = bytes;
    rmpv::decode::read_value_with_max_depth(&mut slice, MAX_DEPTH).map_err(|e| invalid(&e.to_string()))
}

/// Steps over one value's bytes, marker by marker, refusing what the spec doesn't hold: a marker msgpack reserves, a
/// map key that isn't a string, an extension but the timestamp, and a number that isn't finite
fn walk(bytes: &mut &[u8], depth: usize) -> std::result::Result<(), String> {
    use rmp::Marker;
    if depth > MAX_DEPTH {
        return Err(format!("it nests deeper than {MAX_DEPTH}"));
    }
    let marker = rmp::decode::read_marker(bytes).map_err(|_| "it ends early".to_owned())?;
    let take = |n: usize, bytes: &mut &[u8]| -> std::result::Result<u64, String> {
        let raw = bytes.get(..n).ok_or("it ends early")?;
        let value = raw.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b));
        *bytes = &bytes[n..];
        Ok(value)
    };
    let skip = |n: usize, bytes: &mut &[u8]| take_slice(n, bytes).map(|_| ());
    let items = |n: usize, bytes: &mut &[u8]| (0..n).try_for_each(|_| walk(bytes, depth + 1));
    let entries = |n: usize, bytes: &mut &[u8]| -> std::result::Result<(), String> {
        for _ in 0..n {
            let key = bytes.first().copied().ok_or("it ends early")?;
            if !matches!(Marker::from_u8(key), Marker::FixStr(_) | Marker::Str8 | Marker::Str16 | Marker::Str32) {
                return Err(format!("a map key is {}, and keys are strings", kind_of_marker(Marker::from_u8(key))));
            }
            walk(bytes, depth + 1)?;
            walk(bytes, depth + 1)?;
        }
        Ok(())
    };
    let extension = |n: usize, bytes: &mut &[u8]| -> std::result::Result<(), String> {
        let ext = take_slice(n + 1, bytes)?;
        let t = ext[0] as i8;
        if t != TIMESTAMP_EXTENSION {
            return Err(format!("it holds extension type {t}, and the only one allowed is the timestamp (-1)"));
        }
        Ok(())
    };
    let not_finite = || Err("it holds a number that isn't finite".to_owned());
    match marker {
        Marker::Reserved => Err("it holds the reserved marker 0xc1".to_owned()),
        Marker::Null | Marker::True | Marker::False | Marker::FixPos(_) | Marker::FixNeg(_) => Ok(()),
        Marker::U8 | Marker::I8 => skip(1, bytes),
        Marker::U16 | Marker::I16 => skip(2, bytes),
        Marker::U32 | Marker::I32 => skip(4, bytes),
        Marker::U64 | Marker::I64 => skip(8, bytes),
        Marker::F32 => {
            if f32::from_bits(take(4, bytes)? as u32).is_finite() {
                Ok(())
            } else {
                not_finite()
            }
        }
        Marker::F64 => {
            if f64::from_bits(take(8, bytes)?).is_finite() {
                Ok(())
            } else {
                not_finite()
            }
        }
        Marker::FixStr(n) => skip(usize::from(n), bytes),
        Marker::Str8 | Marker::Bin8 => {
            let n = take(1, bytes)? as usize;
            skip(n, bytes)
        }
        Marker::Str16 | Marker::Bin16 => {
            let n = take(2, bytes)? as usize;
            skip(n, bytes)
        }
        Marker::Str32 | Marker::Bin32 => {
            let n = take(4, bytes)? as usize;
            skip(n, bytes)
        }
        Marker::FixArray(n) => items(usize::from(n), bytes),
        Marker::Array16 => {
            let n = take(2, bytes)? as usize;
            items(n, bytes)
        }
        Marker::Array32 => {
            let n = take(4, bytes)? as usize;
            items(n, bytes)
        }
        Marker::FixMap(n) => entries(usize::from(n), bytes),
        Marker::Map16 => {
            let n = take(2, bytes)? as usize;
            entries(n, bytes)
        }
        Marker::Map32 => {
            let n = take(4, bytes)? as usize;
            entries(n, bytes)
        }
        Marker::FixExt1 => extension(1, bytes),
        Marker::FixExt2 => extension(2, bytes),
        Marker::FixExt4 => extension(4, bytes),
        Marker::FixExt8 => extension(8, bytes),
        Marker::FixExt16 => extension(16, bytes),
        Marker::Ext8 => {
            let n = take(1, bytes)? as usize;
            extension(n, bytes)
        }
        Marker::Ext16 => {
            let n = take(2, bytes)? as usize;
            extension(n, bytes)
        }
        Marker::Ext32 => {
            let n = take(4, bytes)? as usize;
            extension(n, bytes)
        }
    }
}

/// The next `n` bytes, stepped over
fn take_slice<'a>(n: usize, bytes: &mut &'a [u8]) -> std::result::Result<&'a [u8], String> {
    if bytes.len() < n {
        return Err("it ends early".to_owned());
    }
    let (head, rest) = bytes.split_at(n);
    *bytes = rest;
    Ok(head)
}

fn kind_of_marker(marker: rmp::Marker) -> &'static str {
    use rmp::Marker;
    match marker {
        Marker::Null => "nil",
        Marker::True | Marker::False => "a boolean",
        Marker::FixPos(_)
        | Marker::FixNeg(_)
        | Marker::U8
        | Marker::U16
        | Marker::U32
        | Marker::U64
        | Marker::I8
        | Marker::I16
        | Marker::I32
        | Marker::I64
        | Marker::F32
        | Marker::F64 => "a number",
        Marker::Bin8 | Marker::Bin16 | Marker::Bin32 => "binary",
        Marker::FixArray(_) | Marker::Array16 | Marker::Array32 => "an array",
        Marker::FixMap(_) | Marker::Map16 | Marker::Map32 => "a map",
        Marker::Reserved => "the reserved marker",
        _ => "an extension",
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
