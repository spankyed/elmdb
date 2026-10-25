//! The key codec: a tuple of parts as bytes whose order is the tuples' order, part by part (`docs/format.md`, Keys).
//!
//! Each part is a tag and its bytes: `null`, `false`, `true`, a number (its float64 big-endian, the sign bit flipped for
//! a positive number and every bit inverted for a negative one, so byte order is numeric order), a string or bytes
//! (each 0 byte written as `00 FF`, then a `00` that ends it, as FoundationDB's tuple layer does, so any byte may appear
//! and `"a"` sorts before `"a\0"`, `"ab"` and `["a", …]`). The keys under a prefix lie between the prefix followed by
//! `TAG_LOW` and the prefix followed by `TAG_HIGH`, so the prefix's own key is outside them.

use crate::error::{Error, ErrorCode, Result};

const NULL: u8 = 0x01;
const FALSE: u8 = 0x02;
const TRUE: u8 = 0x03;
const NUMBER: u8 = 0x04;
const STRING: u8 = 0x05;
const BYTES: u8 = 0x06;
/// Below every part's tag
pub const TAG_LOW: u8 = 0x01;
/// Above every part's tag
pub const TAG_HIGH: u8 = 0x07;
const ESCAPE: u8 = 0xFF;

/// One part of a key
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Null,
    Bool(bool),
    /// Any number but NaN, which has no place in an order; `-0` is stored as `0`
    Number(f64),
    String(String),
    Bytes(Vec<u8>),
}

impl From<&str> for Part {
    fn from(s: &str) -> Self {
        Part::String(s.to_owned())
    }
}

impl From<String> for Part {
    fn from(s: String) -> Self {
        Part::String(s)
    }
}

impl From<f64> for Part {
    fn from(n: f64) -> Self {
        Part::Number(n)
    }
}

impl From<u64> for Part {
    fn from(n: u64) -> Self {
        Part::Number(n as f64)
    }
}

impl From<bool> for Part {
    fn from(b: bool) -> Self {
        Part::Bool(b)
    }
}

/// Writes keys into one buffer it reuses
#[derive(Default)]
pub struct KeyWriter {
    bytes: Vec<u8>,
}

impl KeyWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a new key, forgetting the last
    pub fn clear(&mut self) -> &mut Self {
        self.bytes.clear();
        self
    }

    pub fn null(&mut self) -> &mut Self {
        self.bytes.push(NULL);
        self
    }

    pub fn bool(&mut self, b: bool) -> &mut Self {
        self.bytes.push(if b { TRUE } else { FALSE });
        self
    }

    /// A number. NaN is refused by `Part`'s constructors' callers; here it panics, since no caller should reach it
    pub fn number(&mut self, n: f64) -> &mut Self {
        assert!(!n.is_nan(), "a key holds no NaN");
        let n = if n == 0.0 { 0.0 } else { n };
        let bits = n.to_bits();
        let ordered = if n.is_sign_negative() { !bits } else { bits ^ (1 << 63) };
        self.bytes.push(NUMBER);
        self.bytes.extend_from_slice(&ordered.to_be_bytes());
        self
    }

    pub fn string(&mut self, s: &str) -> &mut Self {
        self.bytes.push(STRING);
        self.escaped(s.as_bytes());
        self
    }

    /// A string's leading characters, left open: the key this writes is a byte prefix of every key whose part here is
    /// a string starting with `s`, which `Read::visit_starting` ranges over
    pub fn string_prefix(&mut self, s: &str) -> &mut Self {
        self.bytes.push(STRING);
        self.escaped_open(s.as_bytes());
        self
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.bytes.push(BYTES);
        self.escaped(b);
        self
    }

    /// The bytes escaped and ended
    fn escaped(&mut self, b: &[u8]) {
        self.escaped_open(b);
        self.bytes.push(0);
    }

    /// The bytes escaped, left open for more
    fn escaped_open(&mut self, b: &[u8]) {
        for &byte in b {
            self.bytes.push(byte);
            if byte == 0 {
                self.bytes.push(ESCAPE);
            }
        }
    }

    pub fn part(&mut self, part: &Part) -> &mut Self {
        match part {
            Part::Null => self.null(),
            Part::Bool(b) => self.bool(*b),
            Part::Number(n) => self.number(*n),
            Part::String(s) => self.string(s),
            Part::Bytes(b) => self.bytes(b),
        }
    }

    /// Appends a raw byte, for a range's bounds (`TAG_LOW`, `TAG_HIGH`)
    pub fn raw(&mut self, byte: u8) -> &mut Self {
        self.bytes.push(byte);
        self
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes.clone()
    }
}

/// A key of these parts
pub fn encode(parts: &[Part]) -> Vec<u8> {
    let mut w = KeyWriter::new();
    for part in parts {
        w.part(part);
    }
    w.to_vec()
}

/// The range of every key under `prefix`: `[prefix·TAG_LOW, prefix·TAG_HIGH)`
pub fn prefix_range(prefix: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut low = prefix.to_vec();
    low.push(TAG_LOW);
    let mut high = prefix.to_vec();
    high.push(TAG_HIGH);
    (low, high)
}

/// A key's parts, or `CORRUPTED` for bytes no key encodes to
pub fn decode(bytes: &[u8]) -> Result<Vec<Part>> {
    let corrupt = || Error::new(ErrorCode::Corrupted, format!("A stored key doesn't decode: {bytes:02x?}"));
    let mut parts = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let tag = bytes[at];
        at += 1;
        match tag {
            NULL => parts.push(Part::Null),
            FALSE => parts.push(Part::Bool(false)),
            TRUE => parts.push(Part::Bool(true)),
            NUMBER => {
                let raw: [u8; 8] = bytes.get(at..at + 8).ok_or_else(corrupt)?.try_into().map_err(|_| corrupt())?;
                let ordered = u64::from_be_bytes(raw);
                let bits = if ordered & (1 << 63) != 0 { ordered ^ (1 << 63) } else { !ordered };
                parts.push(Part::Number(f64::from_bits(bits)));
                at += 8;
            }
            STRING | BYTES => {
                let mut out = Vec::new();
                loop {
                    let byte = *bytes.get(at).ok_or_else(corrupt)?;
                    at += 1;
                    if byte == 0 {
                        if bytes.get(at) == Some(&ESCAPE) {
                            out.push(0);
                            at += 1;
                        } else {
                            break;
                        }
                    } else {
                        out.push(byte);
                    }
                }
                parts.push(if tag == STRING {
                    Part::String(String::from_utf8(out).map_err(|_| corrupt())?)
                } else {
                    Part::Bytes(out)
                });
            }
            _ => return Err(corrupt()),
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use proptest::prelude::*;

    use super::*;

    /// The order keys are meant to have: part by part, a shorter tuple before a longer one it begins
    fn tuple_order(a: &[Part], b: &[Part]) -> Ordering {
        fn rank(p: &Part) -> u8 {
            match p {
                Part::Null => 0,
                Part::Bool(false) => 1,
                Part::Bool(true) => 2,
                Part::Number(_) => 3,
                Part::String(_) => 4,
                Part::Bytes(_) => 5,
            }
        }
        for (x, y) in a.iter().zip(b) {
            let o = rank(x).cmp(&rank(y)).then_with(|| match (x, y) {
                (Part::Number(m), Part::Number(n)) => m.partial_cmp(n).unwrap(),
                (Part::String(m), Part::String(n)) => m.as_bytes().cmp(n.as_bytes()),
                (Part::Bytes(m), Part::Bytes(n)) => m.cmp(n),
                _ => Ordering::Equal,
            });
            if o != Ordering::Equal {
                return o;
            }
        }
        a.len().cmp(&b.len())
    }

    fn part() -> impl Strategy<Value = Part> {
        prop_oneof![
            Just(Part::Null),
            any::<bool>().prop_map(Part::Bool),
            any::<f64>().prop_filter("no NaN", |n| !n.is_nan()).prop_map(|n| Part::Number(if n == 0.0 {
                0.0
            } else {
                n
            })),
            // Short strings over a few characters, NUL among them, so tuples share prefixes
            "[a\\x00\u{e9}b]{0,4}".prop_map(Part::String),
            prop::collection::vec(prop_oneof![Just(0u8), Just(1u8), Just(255u8)], 0..4).prop_map(Part::Bytes),
        ]
    }

    proptest! {
        #[test]
        fn byte_order_is_tuple_order(a in prop::collection::vec(part(), 0..4), b in prop::collection::vec(part(), 0..4)) {
            prop_assert_eq!(encode(&a).cmp(&encode(&b)), tuple_order(&a, &b));
        }

        #[test]
        fn decodes_what_it_encodes(parts in prop::collection::vec(part(), 0..5)) {
            prop_assert_eq!(decode(&encode(&parts)).unwrap(), parts);
        }

        #[test]
        fn a_prefix_range_holds_exactly_the_keys_that_extend_the_prefix(
            prefix in prop::collection::vec(part(), 1..3),
            other in prop::collection::vec(part(), 0..4),
        ) {
            let p = encode(&prefix);
            let (low, high) = prefix_range(&p);
            let key = encode(&other);
            let extends = other.len() > prefix.len() && other[..prefix.len()] == prefix[..];
            prop_assert_eq!(low.as_slice() <= key.as_slice() && key.as_slice() < high.as_slice(), extends);
        }
    }

    #[test]
    fn negative_zero_is_zero_and_strings_hold_nul() {
        assert_eq!(encode(&[Part::Number(-0.0)]), encode(&[Part::Number(0.0)]));
        let parts = vec![Part::from("a\0b"), Part::from("a")];
        assert_eq!(decode(&encode(&parts)).unwrap(), parts);
    }

    #[test]
    fn refuses_bytes_no_key_encodes_to() {
        assert_eq!(decode(&[0x09]).unwrap_err().code, ErrorCode::Corrupted);
        assert_eq!(decode(&[STRING, b'a']).unwrap_err().code, ErrorCode::Corrupted);
        assert_eq!(decode(&[NUMBER, 0, 0]).unwrap_err().code, ErrorCode::Corrupted);
    }
}
