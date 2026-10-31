//! Declared constraints audited from primary values and relation rows. Derived indexes cannot establish validity.

use super::{Violation, entities_of, key_text};
use crate::engine::unit::Read;
use crate::engine::{
    entities::{self, ValueIndex, value_indexes},
    ids, keys, relations, values,
};
use crate::error::{Error, ErrorCode, Result};
use crate::schema::{Cardinality, Schema};
use rmpv::Value;
use std::collections::{BTreeMap, HashMap};

/// References in lists have the same missing-target audit contract as a scalar reference. Objects and `any` are
/// opaque: only a declared ref type assigns entity-id meaning to a string.
fn audit_references(
    r: &impl Read,
    subject: &str,
    path: &str,
    kind: &crate::schema::ValueType,
    value: &Value,
    out: &mut Vec<Violation>,
) -> Result<()> {
    match (kind, value) {
        (crate::schema::ValueType::Ref { .. }, Value::String(target)) => {
            if let Some(target) = target.as_str()
                && entities::record(r, target)?.is_none()
            {
                out.push(Violation {
                    subject: subject.to_owned(),
                    message: format!("{path} names {target}, which no entity has"),
                });
            }
        }
        (crate::schema::ValueType::List { of }, Value::Array(items)) => {
            for (index, item) in items.iter().enumerate() {
                audit_references(r, subject, &format!("{path}[{index}]"), of, item, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn violations(r: &impl Read) -> Result<Vec<Violation>> {
    let schema: &Schema = r.schema();
    let mut out = Vec::new();
    for e in entities_of(r)? {
        let t = ids::type_of(&e.id);
        let Some(def) = schema.types.get(t) else { continue };
        let fields = entities::fields(r, &e.id)?;
        let held: std::collections::HashSet<&str> = fields.iter().map(|(f, _)| f.as_str()).collect();
        for (name, field) in &def.fields {
            if !field.optional && !held.contains(name.as_str()) {
                out.push(Violation { subject: e.id.clone(), message: format!("lacks {name}, which every {t} holds") });
            }
        }
        for (name, bytes) in &fields {
            if entities::field::RESERVED.contains(&name.as_str()) {
                continue;
            }
            let value = values::decode(bytes)?;
            match def.fields.get(name) {
                Some(field) => {
                    if let Err(e2) = values::check_field(t, name, field, &value) {
                        out.push(Violation { subject: e.id.clone(), message: e2.message });
                    }
                    audit_references(r, &e.id, name, &field.value, &value, &mut out)?;
                }
                None if def.additional_fields == crate::schema::AdditionalFields::Refuse => {
                    out.push(Violation {
                        subject: e.id.clone(),
                        message: format!("holds {name}, which {t} doesn't declare"),
                    });
                }
                None => {}
            }
        }
    }
    // Uniqueness belongs to primary values, including values the index cannot file. Group entities by type once,
    // then canonicalize values for ordered grouping without a quadratic pairwise scan of ordinary values.
    let mut by_type: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entity in entities_of(r)? {
        by_type.entry(ids::type_of(&entity.id).to_owned()).or_default().push(entity.id);
    }
    for (entity_type, ids) in by_type {
        for index in value_indexes(schema, &entity_type).into_iter().filter(|i| i.unique) {
            audit_unique(r, &entity_type, &index, &ids, &mut out)?;
        }
    }
    // Cardinality counts far ends, not edges (`schema.md`, Relations): a second edge of a kind that isn't unique, to
    // the same far end, leaves a target one source, as the write that allowed it found
    let mut targets_of: BTreeMap<(String, String), std::collections::BTreeSet<String>> = BTreeMap::new();
    let mut sources_of: BTreeMap<(String, String), std::collections::BTreeSet<String>> = BTreeMap::new();
    let mut pairs: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    let mut graphs: BTreeMap<String, PrimaryGraph> = BTreeMap::new();
    for (key, bytes) in r.scan(&keys::all_relations())? {
        let relation = key_text(&key);
        let s: relations::Stored = rmp_serde::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::Corrupted, format!("A relation's row doesn't read: {e}")))?;
        for end in [&s.source, &s.target] {
            if entities::record(r, end)?.is_none() {
                out.push(Violation { subject: relation.clone(), message: format!("joins {end}, which no entity has") });
            }
        }
        if s.source == s.target {
            out.push(Violation {
                subject: relation.clone(),
                message: format!("a {} relation links {} to itself", s.kind, s.source),
            });
        }
        // A weight a write would refuse, which a load can bring; one kept on a kind no longer weighted is data nothing
        // reads, not a violation
        if let Some(weight) = s.weight.filter(|w| !(0.0..=relations::MAX_WEIGHT).contains(w)) {
            out.push(Violation {
                subject: relation.clone(),
                message: format!("a {} relation weighs {weight}, where a weight is a number from 0 to 2^960", s.kind),
            });
        }
        if let Some(def) = schema.relations.get(&s.kind) {
            if let Some(endpoints) = &def.endpoints {
                let (source, target) = (ids::type_of(&s.source), ids::type_of(&s.target));
                if !endpoints.iter().any(|end| end.source == source && end.target == target) {
                    out.push(Violation {
                        subject: relation.clone(),
                        message: format!("{} doesn't join a {source} to a {target}", s.kind),
                    });
                }
            }
            match &s.info {
                Some(info) => {
                    if let Err(error) = values::check_field("relation", &s.kind, &def.info, info) {
                        out.push(Violation { subject: relation.clone(), message: error.message });
                    }
                    audit_references(r, &relation, "info", &def.info.value, info, &mut out)?;
                }
                None if !def.info.optional => out.push(Violation {
                    subject: relation.clone(),
                    message: format!("{} relation lacks required info", s.kind),
                }),
                None => {}
            }
            if def.unique {
                pairs.entry((s.kind.clone(), s.source.clone(), s.target.clone())).or_default().push(relation.clone());
            }
            if def.acyclic {
                graphs
                    .entry(s.kind.clone())
                    .or_default()
                    .entry(s.source.clone())
                    .or_default()
                    .push((s.target.clone(), relation.clone()));
            }
        }
        targets_of.entry((s.source.clone(), s.kind.clone())).or_default().insert(s.target.clone());
        sources_of.entry((s.target.clone(), s.kind.clone())).or_default().insert(s.source.clone());
    }
    for ((kind, source, target), relations) in pairs {
        if relations.len() > 1 {
            out.push(Violation {
                subject: relations.join(", "),
                message: format!("duplicate {kind} edges join {source} to {target}, which is a unique pair"),
            });
        }
    }
    for (kind, graph) in graphs {
        audit_cycles(&kind, &graph, &mut out);
    }
    for ((end, kind), far) in &sources_of {
        let one_source = schema
            .relations
            .get(kind)
            .is_some_and(|def| matches!(def.cardinality, Cardinality::OneToMany | Cardinality::OneToOne));
        if one_source && far.len() > 1 {
            out.push(Violation {
                subject: end.clone(),
                message: format!("has {} {kind} sources, and a {kind} target has one", far.len()),
            });
        }
    }
    for ((end, kind), far) in &targets_of {
        let one_target = schema
            .relations
            .get(kind)
            .is_some_and(|def| matches!(def.cardinality, Cardinality::ManyToOne | Cardinality::OneToOne));
        if one_target && far.len() > 1 {
            out.push(Violation {
                subject: end.clone(),
                message: format!("has {} {kind} targets, and a {kind} source has one", far.len()),
            });
        }
    }
    Ok(out)
}

/// Canonicalize numeric representations and unordered map fields according to values::same. Sorting by field name
/// alone cannot canonicalize repeated keys' value order, so those maps use an exact pairwise fallback.
fn unique_canonical(value: &Value) -> Option<Value> {
    if let Some(number) = values::as_number(value) {
        return Some(Value::F64(if number == 0.0 { 0.0 } else { number }));
    }
    match value {
        Value::Array(items) => items.iter().map(unique_canonical).collect::<Option<Vec<_>>>().map(Value::Array),
        Value::Map(fields) => {
            let mut fields: Vec<_> = fields
                .iter()
                .map(|(key, value)| Some((values::encode(key), key.clone(), unique_canonical(value)?)))
                .collect::<Option<_>>()?;
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            if fields.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return None;
            }
            Some(Value::Map(fields.into_iter().map(|(_, key, value)| (key, value)).collect()))
        }
        other => Some(other.clone()),
    }
}

fn audit_unique(
    r: &impl Read,
    entity_type: &str,
    index: &ValueIndex,
    ids: &[String],
    out: &mut Vec<Violation>,
) -> Result<()> {
    let mut groups: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    let mut ordinary = Vec::new();
    let mut exceptional = Vec::new();
    for id in ids {
        let fields = index.fields.iter().map(|field| entities::get(r, id, field)).collect::<Result<Vec<_>>>()?;
        for tuple in entities::unique_keys(&fields) {
            let canonical = tuple.iter().map(unique_canonical).collect::<Option<Vec<_>>>();
            if let Some(canonical) = canonical {
                let group = groups.entry(values::encode(&Value::Array(canonical))).or_default();
                if group.last() != Some(id) {
                    group.push(id.clone());
                }
                ordinary.push((id, tuple));
            } else {
                exceptional.push((id, tuple));
            }
        }
    }
    let message = || format!("hold the same {entity_type}.{} value, which is unique", index.name);
    for ids in groups.into_values().filter(|ids| ids.len() > 1) {
        out.push(Violation { subject: ids.join(", "), message: message() });
    }
    for (position, (id, tuple)) in exceptional.iter().enumerate() {
        for (other, held) in ordinary.iter().chain(exceptional[..position].iter()) {
            if id != other && tuple.len() == held.len() && tuple.iter().zip(held).all(|(a, b)| values::same(a, b)) {
                out.push(Violation { subject: format!("{other}, {id}"), message: message() });
            }
        }
    }
    Ok(())
}

/// Primary adjacency, never derived edge keys: a damaged index must not hide a cycle.
pub(super) type PrimaryGraph = HashMap<String, Vec<(String, String)>>;

/// Iterative three-color DFS avoids recursion on a long chain. Each edge is visited once; root sorting makes
/// report order deterministic. Hash lookups are expected constant time; sorting costs O(V log V).
pub(super) fn audit_cycles(kind: &str, graph: &PrimaryGraph, out: &mut Vec<Violation>) {
    let mut color: HashMap<&str, u8> = HashMap::new();
    let mut roots: Vec<&str> = graph.keys().map(String::as_str).collect();
    roots.sort_unstable();
    for root in roots {
        if color.contains_key(root) {
            continue;
        }
        color.insert(root, 1);
        let mut stack = vec![(root, 0usize)];
        while let Some((node, next)) = stack.last_mut() {
            let edges = graph.get(*node).map(Vec::as_slice).unwrap_or_default();
            let Some((target, relation)) = edges.get(*next) else {
                color.insert(*node, 2);
                stack.pop();
                continue;
            };
            *next += 1;
            match color.get(target.as_str()).copied() {
                Some(1) => out.push(Violation {
                    subject: relation.clone(),
                    message: format!("{kind} relation closes a cycle from {node} to {target}"),
                }),
                Some(2) => {}
                _ => {
                    color.insert(target, 1);
                    stack.push((target, 0));
                }
            }
        }
    }
}
