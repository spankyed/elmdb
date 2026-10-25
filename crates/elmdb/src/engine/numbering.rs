//! Numbering: short codes (`<prefix>-<n>`) and labels (`<base> <n>`). A number comes from a stored sequence (`p/s`), so
//! it is never handed out twice, a destroyed entity's included. A sequence's first use starts after the highest number
//! the type's entities already hold, and a number an entity took for itself is skipped. The sequence is written in the
//! unit that takes it, so a unit rolled back gives its numbers back.

use std::collections::HashSet;

use rmpv::Value;

use crate::error::Result;
use crate::format::key::Part;

use super::entities::{self, field};
use super::keys::{self, SHORT_CODE_INDEX};
use super::unit::{Read, Txn};
use super::values;

fn number_in(value: &str, head: &str) -> Option<u64> {
    let rest = value.strip_prefix(head)?;
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) && !rest.starts_with('0'))
        .then(|| rest.parse().ok())?
}

fn read_sequence(t: &impl Read, name: &str) -> Result<Option<u64>> {
    t.get(&keys::sequence(name))?.map(|b| values::decode(&b).map(|v| v.as_u64().unwrap_or(1))).transpose()
}

fn write_sequence(t: &mut Txn<'_, '_>, name: &str, next: u64) -> Result<()> {
    t.put(&keys::sequence(name), &values::encode(&next.into()))
}

/// Whether an entity of the type holds this short code, read from the short code's index
fn code_taken(t: &impl Read, entity_type: &str, code: &str) -> Result<bool> {
    let mut taken = false;
    t.visit(&keys::index_range(entity_type, SHORT_CODE_INDEX, &[Part::String(code.to_owned())]), |_, _| {
        taken = true;
        false
    })?;
    Ok(taken)
}

/// The next short code of the type: `<prefix>-<n>`
pub fn next_short_code(t: &mut Txn<'_, '_>, entity_type: &str, prefix: &str) -> Result<String> {
    let name = format!("shortCode:{entity_type}");
    let head = format!("{prefix}-");
    let mut next = match read_sequence(t, &name)? {
        Some(next) => next,
        None => {
            // The one walk of the type's codes: a sequence starts after the highest its entities hold
            let mut highest = 0;
            for (key, _) in t.scan(&keys::index_range(entity_type, SHORT_CODE_INDEX, &[]))? {
                if let Some(Part::String(code)) = crate::format::key::decode(&key)?.get(3) {
                    highest = highest.max(number_in(code, &head).unwrap_or(0));
                }
            }
            highest + 1
        }
    };
    while code_taken(t, entity_type, &format!("{head}{next}"))? {
        next += 1;
    }
    write_sequence(t, &name, next + 1)?;
    Ok(format!("{head}{next}"))
}

/// Whether a short code is the type's `<prefix>-<n>`
pub fn is_short_code_of(code: &str, prefix: &str) -> bool {
    number_in(code, &format!("{prefix}-")).is_some()
}

/// The next label of the type: `<base> <n>`, no entity of the type holding it
pub fn next_label(t: &mut Txn<'_, '_>, entity_type: &str, base: &str) -> Result<String> {
    let name = format!("label:{entity_type}:{base}");
    let head = format!("{base} ");
    // Labels are read from the type's rows, live and trashed, since no index need cover them
    let mut held = HashSet::new();
    for trashed in [false, true] {
        for (_, id) in t.scan(&keys::type_range(trashed, entity_type))? {
            let id = values::decode(&id)?;
            if let Some(Value::String(label)) = entities::get(t, id.as_str().unwrap_or_default(), field::LABEL)?
                && let Some(label) = label.as_str()
            {
                held.insert(label.to_owned());
            }
        }
    }
    let mut next = match read_sequence(t, &name)? {
        Some(next) => next,
        None => held.iter().filter_map(|l| number_in(l, &head)).max().unwrap_or(0) + 1,
    };
    while held.contains(&format!("{head}{next}")) {
        next += 1;
    }
    write_sequence(t, &name, next + 1)?;
    Ok(format!("{head}{next}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_number_only_from_its_own_form() {
        assert_eq!(number_in("THR-12", "THR-"), Some(12));
        assert_eq!(number_in("THR-012", "THR-"), None);
        assert_eq!(number_in("THR-", "THR-"), None);
        assert_eq!(number_in("NTE-3", "THR-"), None);
        assert!(is_short_code_of("THR-1", "THR"));
        assert!(!is_short_code_of("THR-x", "THR"));
    }
}
