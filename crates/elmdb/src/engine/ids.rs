//! Ids. An entity's id is `<Type>-<suffix>`: its type is read from it and stored nowhere else, so none can disagree. A
//! relation's is `Relation-<suffix>`, which no entity type may be named.

/// The type an entity's id names
pub fn type_of(id: &str) -> &str {
    id.split_once('-').map_or(id, |(t, _)| t)
}

/// A new entity id: the type, a base-36 time and 64 random bits
pub fn new_entity_id(entity_type: &str, now: f64) -> String {
    let random = getrandom::u64().expect("the OS supplies random bytes");
    format!("{entity_type}-{}{}", base36(now.max(0.0) as u64), base36(random))
}

/// A new relation id: the time, then the place in the store's sequence, so ids sort in the order relations were made
pub fn new_relation_id(now: f64, seq: u64) -> String {
    format!("Relation-{}{:0>8}", base36(now.max(0.0) as u64), base36(seq))
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_owned();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("base-36 digits are ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_name_their_type_and_sort_relations_by_place() {
        let id = new_entity_id("Note", 1_700_000_000_000.0);
        assert!(id.starts_with("Note-"));
        assert_eq!(type_of(&id), "Note");
        assert_eq!(type_of("Note"), "Note");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        let a = new_relation_id(5.0, 9);
        let b = new_relation_id(5.0, 10);
        assert!(a < b);
    }
}
