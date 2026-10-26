//! What opening a database with a new schema does, change by change (`docs/architecture/schema.md`, Changing a schema). A change is
//! applied, builds or drops derived data, keeps the data of a declaration that went, enforces a tightened rule on
//! writes from then on, or is refused.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::model::*;

/// One difference between the stored schema and the given one, and what opening does about it
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
pub struct SchemaChange {
    /// Where in the schema, as a problem's path names it: `types.Message.fields.text`
    pub path: String,
    pub change: Change,
    pub effect: Effect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Change {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Effect {
    /// Taken as given, with nothing to build or check
    Applied,
    /// A new index, built in the background; plans walk the rows until it's built
    Builds,
    /// An index rebuilt in the background under its name
    Rebuilds,
    /// An index's derived data deleted
    Drops,
    /// The declaration went and its data stays: writes to it are refused, and `admin` reads and purges it
    KeepsData,
    /// A tightened rule, enforced on every write from then on; `check()` reports existing rows that break it
    Enforced,
    /// Not allowed: the open fails, naming it
    Refused,
}

/// What the stored data holds that a change answers to and neither schema says: rows a declaration now applies to,
/// and what a removed declaration left, which stays until it is purged
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Held {
    /// The types with entities, live or trashed
    pub types: BTreeSet<String>,
    /// The short code prefix of each type whose entities hold codes
    pub short_codes: BTreeMap<String, String>,
    /// Of the relation kinds the given schema declares and the stored one doesn't, those with relations stored
    pub kinds: BTreeSet<String>,
}

/// Every change from `stored` to `given`, over the data `held`, in path order
pub fn diff(stored: &Schema, given: &Schema, held: &Held) -> Vec<SchemaChange> {
    let mut out = Changes(Vec::new());
    if stored.collation != given.collation {
        // A value index holds its values as written; the collation orders `orderBy` as a query runs
        out.push("collation", Change::Changed, Effect::Applied);
    }
    // A declaration added over data a removed one left holds that data to its rules from then on
    let over = |data: bool| if data { Effect::Enforced } else { Effect::Applied };
    keyed(&stored.roles, &given.roles, "roles", &mut out, |_, _| Effect::Applied, |_, _, _, _| {});
    let added_kind = |name: &str, _: &RelationDef| over(held.kinds.contains(name));
    keyed(&stored.relations, &given.relations, "relations", &mut out, added_kind, |at, old, new, out| {
        let tightened = !widens_cardinality(old.cardinality, new.cardinality)
            || (new.acyclic && !old.acyclic)
            || (new.unique && !old.unique)
            || !widens_endpoints(old.endpoints.as_deref(), new.endpoints.as_deref())
            || !widens_field(&old.info, &new.info);
        out.push(at, Change::Changed, if tightened { Effect::Enforced } else { Effect::Applied });
    });
    let added_type = |name: &str, _: &TypeDef| over(held.types.contains(name));
    keyed(&stored.types, &given.types, "types", &mut out, added_type, |at, old, new, out| {
        let has_rows = held.types.contains(at.strip_prefix("types.").unwrap_or(at));
        match (&old.short_code, &new.short_code) {
            (Some(a), Some(b)) if a != b => {
                out.push(&format!("{at}.shortCode.prefix"), Change::Changed, Effect::Refused)
            }
            (Some(_), None) => out.push(&format!("{at}.shortCode"), Change::Removed, Effect::Refused),
            (None, Some(_)) => out.push(&format!("{at}.shortCode"), Change::Added, Effect::Applied),
            _ => {}
        }
        if old.additional_fields != new.additional_fields {
            let effect =
                if new.additional_fields == AdditionalFields::Refuse { Effect::Enforced } else { Effect::Applied };
            out.push(&format!("{at}.additionalFields"), Change::Changed, effect);
        }
        if old.declared_by != new.declared_by {
            out.push(&format!("{at}.declaredBy"), Change::Changed, Effect::Applied);
        }
        // A row holds no value of a field its type refused, which breaks a field only when it's required, and anything
        // in a field its type allowed
        let breaks_rows = |field: &FieldDef| match old.additional_fields {
            AdditionalFields::Refuse => !field.optional,
            AdditionalFields::Allow => !widens_field(&any_info(), field),
        };
        let added_field = |_: &str, field: &FieldDef| over(has_rows && breaks_rows(field));
        keyed(&old.fields, &new.fields, &format!("{at}.fields"), out, added_field, |at, old, new, out| {
            let effect = if widens_field(old, new) { Effect::Applied } else { Effect::Enforced };
            out.push(at, Change::Changed, effect);
        });
        let index_at = format!("{at}.indexes");
        for (name, old_index) in &old.indexes {
            let path = format!("{index_at}.{name}");
            match new.indexes.get(name) {
                None => out.push(&path, Change::Removed, Effect::Drops),
                Some(new_index) if new_index != old_index => {
                    // A unique index made so is rebuilt and enforced; the rebuild refuses to build over duplicates
                    out.push(&path, Change::Changed, Effect::Rebuilds);
                    if let (IndexDef::Value { unique: false, .. }, IndexDef::Value { unique: true, .. }) =
                        (old_index, new_index)
                    {
                        out.push(&format!("{path}.unique"), Change::Changed, Effect::Enforced);
                    }
                }
                Some(_) => {}
            }
        }
        for name in new.indexes.keys() {
            if !old.indexes.contains_key(name) {
                out.push(&format!("{index_at}.{name}"), Change::Added, Effect::Builds);
            }
        }
    });
    // A code is an entity's identity, so the codes stored entities hold keep their prefix: a type declared again keeps
    // its own, and no other type takes it, whose numbering would start again at 1
    for (name, def) in &given.types {
        let at = format!("types.{name}.shortCode");
        let holds = held.short_codes.get(name);
        if !stored.types.contains_key(name) {
            match (holds, &def.short_code) {
                (Some(held), Some(code)) if *held != code.prefix => {
                    out.push(&format!("{at}.prefix"), Change::Changed, Effect::Refused)
                }
                (Some(_), None) => out.push(&at, Change::Removed, Effect::Refused),
                _ => {}
            }
        }
        if let Some(code) = &def.short_code
            && held.short_codes.iter().any(|(other, prefix)| other != name && *prefix == code.prefix)
        {
            out.push(&format!("{at}.prefix"), Change::Added, Effect::Refused);
        }
    }
    // An added type's indexes are built too
    for (name, def) in &given.types {
        if !stored.types.contains_key(name) {
            for index in def.indexes.keys() {
                out.push(&format!("types.{name}.indexes.{index}"), Change::Added, Effect::Builds);
            }
        }
    }
    // A removed type's indexes go with it
    for (name, def) in &stored.types {
        if !given.types.contains_key(name) {
            for index in def.indexes.keys() {
                out.push(&format!("types.{name}.indexes.{index}"), Change::Removed, Effect::Drops);
            }
        }
    }
    let mut changes = out.0;
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

struct Changes(Vec<SchemaChange>);

impl Changes {
    fn push(&mut self, path: &str, change: Change, effect: Effect) {
        self.0.push(SchemaChange { path: path.to_owned(), change, effect });
    }
}

/// Added and removed entries of a keyed map, `added` saying what adding each does, and `changed` called for each kept
/// one that differs
fn keyed<T: PartialEq>(
    old: &BTreeMap<String, T>,
    new: &BTreeMap<String, T>,
    at: &str,
    out: &mut Changes,
    added: impl Fn(&str, &T) -> Effect,
    mut changed: impl FnMut(&str, &T, &T, &mut Changes),
) {
    for (name, old_def) in old {
        let path = format!("{at}.{name}");
        match new.get(name) {
            None => out.push(&path, Change::Removed, Effect::KeepsData),
            Some(new_def) if new_def != old_def => changed(&path, old_def, new_def, out),
            Some(_) => {}
        }
    }
    for (name, new_def) in new {
        if !old.contains_key(name) {
            out.push(&format!("{at}.{name}"), Change::Added, added(name, new_def));
        }
    }
}

/// Whether `new` accepts every value `old` accepted
fn widens_field(old: &FieldDef, new: &FieldDef) -> bool {
    (new.optional || !old.optional) && (new.nullable || !old.nullable) && widens_value(&old.value, &new.value)
}

fn widens_value(old: &ValueType, new: &ValueType) -> bool {
    use ValueType::*;
    match (old, new) {
        (_, Any) => true,
        (Integer, Number) => true,
        (Enum { values: a }, Enum { values: b }) => a.iter().all(|v| b.contains(v)),
        (Enum { .. }, String) => true,
        (Ref { to: a }, Ref { to: b }) => a.iter().all(|t| b.contains(t)),
        (Ref { .. }, String) => true,
        (List { of: a }, List { of: b }) => widens_value(a, b),
        (a, b) => a == b,
    }
}

/// Whether `new` allows every edge `old` allowed: many on each side at least where `old` had many
fn widens_cardinality(old: Cardinality, new: Cardinality) -> bool {
    let sides = |c| match c {
        Cardinality::OneToOne => (false, false),
        Cardinality::OneToMany => (false, true),
        Cardinality::ManyToOne => (true, false),
        Cardinality::ManyToMany => (true, true),
    };
    let ((old_sources, old_targets), (new_sources, new_targets)) = (sides(old), sides(new));
    (new_sources || !old_sources) && (new_targets || !old_targets)
}

fn widens_endpoints(old: Option<&[Endpoint]>, new: Option<&[Endpoint]>) -> bool {
    match (old, new) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(a), Some(b)) => a.iter().all(|e| b.contains(e)),
    }
}
