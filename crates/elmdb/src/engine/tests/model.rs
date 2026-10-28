//! The model harness (`docs/architecture/testing.md`), ported from apack's `tests/_harness/`: random writes over a small pool of ids,
//! so they collide, refer to each other and are refused, checked after every operation against a reference model kept
//! as plainly as a model can be. The engine must hold what the model holds, refuse what it refuses, and keep every
//! derived key in step: a type's live and trashed entities in creation order, its count, each value index's entries,
//! the role index, each entity's edges out and in, and what a text index matches.

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::{Value as Json, json};

use super::{edges as stored_edges, open, row};
use crate::Database;
use crate::engine::unit::Read;
use crate::engine::{entities, keys, relations, values};
use crate::format::key::{Part, decode as decode_key};

const IDS: [&str; 6] = ["Task-1", "Task-2", "Task-3", "Task-4", "Note-1", "Note-2"];
const KEYS: [&str; 3] = ["a", "b", "c"];
const KINDS: [&str; 2] = ["refs", "blocks"];
const ROLES: [&str; 2] = ["r1", "r2"];
/// The engine's own fields, which the model leaves out of a row
const ENGINE_FIELDS: [&str; 5] = ["createdAt", "updatedAt", "shortCode", "trashedAt", "role"];

/// One write the workload makes, as data: the model and the engine each apply it
#[derive(Debug, Clone)]
enum Step {
    Create {
        id: &'static str,
        fields: BTreeMap<&'static str, Json>,
    },
    Set {
        id: &'static str,
        key: &'static str,
        value: Json,
    },
    Append {
        id: &'static str,
        key: &'static str,
        value: i64,
    },
    Delete {
        id: &'static str,
        key: &'static str,
    },
    Link {
        id: &'static str,
        kind: &'static str,
        target: &'static str,
    },
    Unlink {
        id: &'static str,
        kind: &'static str,
        target: &'static str,
    },
    Destroy {
        id: &'static str,
    },
    Trash {
        id: &'static str,
    },
    Restore {
        id: &'static str,
    },
    Grant {
        id: &'static str,
        role: &'static str,
    },
    Revoke {
        id: &'static str,
        role: &'static str,
    },
    Upsert {
        id: &'static str,
        fields: BTreeMap<&'static str, Json>,
    },
    Merge {
        id: &'static str,
        key: &'static str,
        value: Json,
    },
    RemoveAt {
        id: &'static str,
        key: &'static str,
        index: u32,
    },
    RemoveValue {
        id: &'static str,
        key: &'static str,
        value: Json,
    },
    /// Grants the role, revoking it from every other id that holds it
    GrantExclusive {
        id: &'static str,
        role: &'static str,
    },
    /// Removes the first relation between the ends, by its id; refused when there is none
    RemoveRelation {
        id: &'static str,
        kind: &'static str,
        target: &'static str,
    },
    /// Moves the first relation between the ends to another target, by its id; refused when there is none
    MoveRelation {
        id: &'static str,
        kind: &'static str,
        target: &'static str,
        to: &'static str,
    },
    TakeLabel {
        id: &'static str,
    },
    /// Writes made as one unit, which then commits or throws; inside another unit, a savepoint whose refusal rolls back
    /// its own writes while the outer one goes on
    Unit {
        steps: Vec<Step>,
        throws: bool,
        durable: bool,
    },
    /// The store refuses the next commit it is asked to make, as a full disk would
    RefuseNext,
}

fn value() -> impl Strategy<Value = Json> {
    prop_oneof![
        4 => "[a-c]{0,8}".prop_map(Json::from),
        // Numbers from as small a pool as a list's elements, so an index lookup finds one often
        4 => (0i64..10).prop_map(Json::from),
        4 => prop::collection::vec(0i64..10, 0..4).prop_map(|v| json!(v)),
        // Large values: a long string no index holds, and a long list an append then grows
        1 => (0u8..10).prop_map(|n| Json::from(n.to_string().repeat(2100))),
        1 => (0i64..10).prop_map(|n| json!(vec![n; 1100])),
        // Maps, which a merge combines and a removeValue matches by their fields
        2 => prop::collection::btree_map("[k-m]", 0i64..3, 0..3).prop_map(|m| json!(m)),
    ]
}

fn id() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&IDS[..])
}
fn key() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&KEYS[..])
}
fn kind() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&KINDS[..])
}
fn role() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&ROLES[..])
}
fn fields() -> impl Strategy<Value = BTreeMap<&'static str, Json>> {
    prop::collection::btree_map(key(), value(), 0..3)
}

/// One write that isn't a unit
fn write() -> BoxedStrategy<Step> {
    prop_oneof![
        3 => (id(), fields()).prop_map(|(id, fields)| Step::Create { id, fields }),
        3 => (id(), key(), value()).prop_map(|(id, key, value)| Step::Set { id, key, value }),
        2 => (id(), key(), 0i64..10).prop_map(|(id, key, value)| Step::Append { id, key, value }),
        1 => (id(), key()).prop_map(|(id, key)| Step::Delete { id, key }),
        3 => (id(), kind(), id()).prop_map(|(id, kind, target)| Step::Link { id, kind, target }),
        1 => (id(), kind(), id()).prop_map(|(id, kind, target)| Step::Unlink { id, kind, target }),
        1 => id().prop_map(|id| Step::Destroy { id }),
        1 => id().prop_map(|id| Step::Trash { id }),
        1 => id().prop_map(|id| Step::Restore { id }),
        1 => (id(), role()).prop_map(|(id, role)| Step::Grant { id, role }),
        1 => (id(), role()).prop_map(|(id, role)| Step::Revoke { id, role }),
        1 => (id(), fields()).prop_map(|(id, fields)| Step::Upsert { id, fields }),
        1 => (id(), key(), value()).prop_map(|(id, key, value)| Step::Merge { id, key, value }),
        1 => (id(), key(), 0u32..3).prop_map(|(id, key, index)| Step::RemoveAt { id, key, index }),
        1 => (id(), key(), prop_oneof![(0i64..10).prop_map(Json::from), prop::collection::btree_map("[k-m]", 0i64..3, 1..2).prop_map(|m| json!(m))]).prop_map(|(id, key, value)| Step::RemoveValue { id, key, value }),
        1 => (id(), role()).prop_map(|(id, role)| Step::GrantExclusive { id, role }),
        1 => (id(), kind(), id()).prop_map(|(id, kind, target)| Step::RemoveRelation { id, kind, target }),
        1 => (id(), kind(), id(), id()).prop_map(|(id, kind, target, to)| Step::MoveRelation { id, kind, target, to }),
        1 => id().prop_map(|id| Step::TakeLabel { id }),
    ]
    .boxed()
}

fn unit_of(steps: BoxedStrategy<Step>) -> BoxedStrategy<Step> {
    (prop::collection::vec(steps, 1..6), any::<bool>(), any::<bool>())
        .prop_map(|(steps, throws, durable)| Step::Unit { steps, throws, durable })
        .boxed()
}

/// Any step: a write, a unit of writes (one of them sometimes a unit of its own), or the store refusing a commit
fn step() -> BoxedStrategy<Step> {
    let unit_write = prop_oneof![4 => write(), 1 => unit_of(write())].boxed();
    prop_oneof![8 => write(), 2 => unit_of(unit_write), 1 => Just(Step::RefuseNext)].boxed()
}

/// Every id created first, with fields of its own, so what follows lands on data; then single steps, and now and then
/// an edge linked, unlinked and linked again, which independent steps over 72 possible edges almost never make
fn workload() -> impl Strategy<Value = Vec<Step>> {
    let prelude = prop::collection::vec(fields(), IDS.len())
        .prop_map(|all| IDS.iter().zip(all).map(|(id, fields)| Step::Create { id, fields }).collect::<Vec<_>>());
    let relink = (id(), kind(), id()).prop_map(|(id, kind, target)| {
        vec![Step::Link { id, kind, target }, Step::Unlink { id, kind, target }, Step::Link { id, kind, target }]
    });
    let chunks = prop::collection::vec(prop_oneof![9 => step().prop_map(|s| vec![s]), 1 => relink], 10..60);
    (prelude, chunks).prop_map(|(mut steps, chunks)| {
        steps.extend(chunks.into_iter().flatten());
        steps
    })
}

/// One entity as the model holds it
#[derive(Debug, Clone, PartialEq)]
struct Entity {
    seq: u64,
    fields: BTreeMap<String, Json>,
    trashed: bool,
    roles: Vec<String>,
}

/// The data as the model holds it
#[derive(Debug, Clone, Default, PartialEq)]
struct Model {
    entities: BTreeMap<String, Entity>,
    /// Each edge with how many relations join its ends: a move can make a second
    edges: BTreeMap<(String, String, String), usize>,
    /// The edges a unit linked, whose targets must exist when it commits
    pending: Vec<(String, String, String)>,
    next_seq: u64,
    refuse_next: bool,
}

/// Why the model refused a step; the engine must refuse it too
type Refusal = &'static str;

impl Model {
    fn exists(&self, id: &str) -> Result<(), Refusal> {
        if self.entities.contains_key(id) { Ok(()) } else { Err("no such entity") }
    }

    /// Applies one write, as inside a unit
    fn apply(&mut self, step: &Step) -> Result<(), Refusal> {
        match step {
            Step::Create { id, fields } => {
                if self.entities.contains_key(*id) {
                    return Err("id in use");
                }
                self.next_seq += 1;
                let fields = fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                self.entities
                    .insert(id.to_string(), Entity { seq: self.next_seq, fields, trashed: false, roles: vec![] });
            }
            Step::Set { id, key, value } => {
                self.exists(id)?;
                self.entities.get_mut(*id).unwrap().fields.insert(key.to_string(), value.clone());
            }
            Step::Append { id, key, value } => {
                self.exists(id)?;
                let fields = &mut self.entities.get_mut(*id).unwrap().fields;
                match fields.get_mut(*key) {
                    None => {
                        fields.insert(key.to_string(), json!([value]));
                    }
                    Some(Json::Array(items)) => items.push(json!(value)),
                    Some(_) => return Err("not a list"),
                }
            }
            Step::Delete { id, key } => {
                self.exists(id)?;
                self.entities.get_mut(*id).unwrap().fields.remove(*key);
            }
            Step::Link { id, kind, target } => {
                self.exists(id)?;
                if id == target {
                    return Err("self link");
                }
                let edge = (id.to_string(), kind.to_string(), target.to_string());
                if !self.edges.contains_key(&edge) {
                    self.edges.insert(edge.clone(), 1);
                    self.pending.push(edge);
                }
            }
            Step::Unlink { id, kind, target } => {
                self.exists(id)?;
                self.edges.remove(&(id.to_string(), kind.to_string(), target.to_string()));
            }
            Step::Upsert { id, fields } => {
                if self.entities.contains_key(*id) {
                    let row = &mut self.entities.get_mut(*id).unwrap().fields;
                    for (k, v) in fields {
                        row.insert(k.to_string(), v.clone());
                    }
                } else {
                    return self.apply(&Step::Create { id, fields: fields.clone() });
                }
            }
            Step::Merge { id, key, value } => {
                self.exists(id)?;
                let row = &mut self.entities.get_mut(*id).unwrap().fields;
                let merged = match (row.get(*key), value) {
                    (Some(Json::Object(current)), Json::Object(new)) => {
                        let mut out = current.clone();
                        out.extend(new.clone());
                        Json::Object(out)
                    }
                    _ => value.clone(),
                };
                row.insert(key.to_string(), merged);
            }
            Step::RemoveAt { id, key, index } => {
                self.exists(id)?;
                let row = &mut self.entities.get_mut(*id).unwrap().fields;
                match row.get_mut(*key) {
                    None => {}
                    Some(Json::Array(items)) => {
                        if (*index as usize) < items.len() {
                            items.remove(*index as usize);
                        }
                    }
                    Some(_) => return Err("not a list"),
                }
            }
            Step::RemoveValue { id, key, value } => {
                self.exists(id)?;
                let row = &mut self.entities.get_mut(*id).unwrap().fields;
                match row.get_mut(*key) {
                    None => {}
                    Some(Json::Array(items)) => {
                        let matches = |e: &Json| match (e, value) {
                            (Json::Object(e), Json::Object(c)) => c.iter().all(|(k, v)| e.get(k) == Some(v)),
                            (Json::Number(a), Json::Number(b)) => a.as_f64() == b.as_f64(),
                            (Json::Array(_) | Json::Object(_), _) => false,
                            (a, b) => a == b,
                        };
                        if let Some(at) = items.iter().position(matches) {
                            items.remove(at);
                        }
                    }
                    Some(_) => return Err("not a list"),
                }
            }
            Step::GrantExclusive { id, role } => {
                self.exists(id)?;
                for (other, e) in self.entities.iter_mut() {
                    if other != id {
                        e.roles.retain(|r| r != role);
                    }
                }
                return self.apply(&Step::Grant { id, role });
            }
            Step::RemoveRelation { id, kind, target } => {
                let edge = (id.to_string(), kind.to_string(), target.to_string());
                match self.edges.get_mut(&edge) {
                    None => return Err("no such relation"),
                    Some(n) if *n > 1 => *n -= 1,
                    Some(_) => {
                        self.edges.remove(&edge);
                    }
                }
            }
            Step::MoveRelation { id, kind, target, to } => {
                let edge = (id.to_string(), kind.to_string(), target.to_string());
                if !self.edges.contains_key(&edge) {
                    return Err("no such relation");
                }
                if id == to {
                    return Err("self link");
                }
                match self.edges.get_mut(&edge) {
                    Some(n) if *n > 1 => *n -= 1,
                    _ => {
                        self.edges.remove(&edge);
                    }
                }
                let moved = (id.to_string(), kind.to_string(), to.to_string());
                *self.edges.entry(moved.clone()).or_insert(0) += 1;
                self.pending.push(moved);
            }
            Step::TakeLabel { .. } => {}
            Step::Destroy { id } => {
                self.exists(id)?;
                self.entities.remove(*id);
                self.edges.retain(|(s, _, t), _| s != id && t != id);
            }
            Step::Trash { id } => {
                self.exists(id)?;
                self.entities.get_mut(*id).unwrap().trashed = true;
            }
            Step::Restore { id } => {
                self.exists(id)?;
                self.entities.get_mut(*id).unwrap().trashed = false;
            }
            Step::Grant { id, role } => {
                self.exists(id)?;
                let roles = &mut self.entities.get_mut(*id).unwrap().roles;
                if !roles.iter().any(|r| r == role) {
                    roles.push(role.to_string());
                }
            }
            Step::Revoke { id, role } => {
                self.exists(id)?;
                self.entities.get_mut(*id).unwrap().roles.retain(|r| r != role);
            }
            Step::Unit { steps, throws, .. } => {
                // A unit inside another: a savepoint
                let saved = self.clone();
                let outcome = steps.iter().try_for_each(|s| self.apply(s));
                if outcome.is_err() || *throws {
                    *self = Model { refuse_next: self.refuse_next, ..saved };
                    return Ok(());
                }
            }
            Step::RefuseNext => unreachable!("only at the top"),
        }
        Ok(())
    }

    /// The deferred check: every edge the unit linked that still exists has its target
    fn check_pending(&self) -> Result<(), Refusal> {
        for edge in &self.pending {
            if self.edges.contains_key(edge) && !self.entities.contains_key(&edge.2) {
                return Err("link target missing at commit");
            }
        }
        Ok(())
    }

    /// Applies a step at the top, where every write commits: what `Database::write` and a committed unit do
    fn step(&mut self, step: &Step) -> Result<(), Refusal> {
        if let Step::RefuseNext = step {
            self.refuse_next = true;
            return Ok(());
        }
        let saved = self.clone();
        self.pending.clear();
        let applied = match step {
            Step::Unit { steps, throws, .. } => {
                let outcome = steps.iter().try_for_each(|s| self.apply(s));
                match (outcome, throws) {
                    (Err(e), _) => Err(e),
                    (Ok(()), true) => Err("the unit threw"),
                    (Ok(()), false) => self.check_pending(),
                }
            }
            write => self.apply(write).and_then(|()| self.check_pending()),
        };
        match applied {
            Ok(()) if self.refuse_next => {
                *self = Model { refuse_next: false, ..saved };
                Err("the store refused the commit")
            }
            Ok(()) => Ok(()),
            Err(e) => {
                // Refused before it committed: nothing was written, and an armed refusal waits for the next commit
                *self = saved;
                Err(e)
            }
        }
    }
}

/// The first relation between the ends, or an id no relation has, so the engine refuses the operation as the model does
fn relation_between(db: &Database, source: &str, kind: &str, target: &str) -> String {
    db.snapshot(|s| relations::between(s, source, kind, target))
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| "Relation-none".to_owned())
}

/// The engine operation of one write
fn op_of(db: &Database, step: &Step) -> Json {
    match step {
        Step::Upsert { id, fields } => {
            json!({ "op": "upsert", "type": crate::engine::ids::type_of(id), "id": id, "fields": fields })
        }
        Step::Merge { id, key, value } => json!({ "op": "merge", "id": id, "field": key, "value": value }),
        Step::RemoveAt { id, key, index } => json!({ "op": "removeAt", "id": id, "field": key, "index": index }),
        Step::RemoveValue { id, key, value } => json!({ "op": "removeValue", "id": id, "field": key, "value": value }),
        Step::GrantExclusive { id, role } => json!({ "op": "grantExclusive", "id": id, "role": role, "among": IDS }),
        Step::RemoveRelation { id, kind, target } => {
            json!({ "op": "removeRelation", "relation": relation_between(db, id, kind, target) })
        }
        Step::MoveRelation { id, kind, target, to } => {
            json!({ "op": "updateRelation", "relation": relation_between(db, id, kind, target), "target": to })
        }
        Step::TakeLabel { id } => {
            json!({ "op": "takeLabel", "type": crate::engine::ids::type_of(id), "base": "Untitled" })
        }
        other => op_of_plain(other),
    }
}

/// The engine operation of one write inside a unit, whose own writes a relation's lookup must see
fn op_of_unit(unit: &mut crate::Unit, step: &Step) -> Json {
    let mut found = |source: &str, kind: &str, target: &str| {
        unit.read_with(|s| relations::between(s, source, kind, target))
            .unwrap()
            .into_iter()
            .next()
            .unwrap_or_else(|| "Relation-none".to_owned())
    };
    match step {
        Step::RemoveRelation { id, kind, target } => {
            json!({ "op": "removeRelation", "relation": found(id, kind, target) })
        }
        Step::MoveRelation { id, kind, target, to } => {
            json!({ "op": "updateRelation", "relation": found(id, kind, target), "target": to })
        }
        other => op_of_plain(other),
    }
}

fn op_of_plain(step: &Step) -> Json {
    match step {
        Step::Upsert { id, fields } => {
            json!({ "op": "upsert", "type": crate::engine::ids::type_of(id), "id": id, "fields": fields })
        }
        Step::Merge { id, key, value } => json!({ "op": "merge", "id": id, "field": key, "value": value }),
        Step::RemoveAt { id, key, index } => json!({ "op": "removeAt", "id": id, "field": key, "index": index }),
        Step::RemoveValue { id, key, value } => json!({ "op": "removeValue", "id": id, "field": key, "value": value }),
        Step::GrantExclusive { id, role } => json!({ "op": "grantExclusive", "id": id, "role": role, "among": IDS }),
        Step::TakeLabel { id } => {
            json!({ "op": "takeLabel", "type": crate::engine::ids::type_of(id), "base": "Untitled" })
        }
        Step::Create { id, fields } => {
            json!({ "op": "create", "type": crate::engine::ids::type_of(id), "id": id, "fields": fields })
        }
        Step::Set { id, key, value } => json!({ "op": "set", "id": id, "fields": { *key: value } }),
        Step::Append { id, key, value } => json!({ "op": "append", "id": id, "field": key, "value": value }),
        Step::Delete { id, key } => json!({ "op": "delete", "id": id, "field": key }),
        Step::Link { id, kind, target } => json!({ "op": "link", "source": id, "kind": kind, "target": target }),
        Step::Unlink { id, kind, target } => json!({ "op": "unlink", "source": id, "kind": kind, "target": target }),
        Step::Destroy { id } => json!({ "op": "destroy", "id": id }),
        Step::Trash { id } => json!({ "op": "trash", "id": id }),
        Step::Restore { id } => json!({ "op": "restore", "id": id }),
        Step::Grant { id, role } => json!({ "op": "grant", "id": id, "role": role }),
        Step::Revoke { id, role } => json!({ "op": "revoke", "id": id, "role": role }),
        Step::RemoveRelation { .. } | Step::MoveRelation { .. } | Step::Unit { .. } | Step::RefuseNext => {
            unreachable!("not a write on its own")
        }
    }
}

/// Applies a step's writes inside an open unit, as the model's `apply` does
fn apply_in(unit: &mut crate::Unit, step: &Step) -> Result<(), crate::Error> {
    match step {
        Step::Unit { steps, throws, .. } => {
            let depth = unit.savepoint()?;
            let outcome = steps.iter().try_for_each(|s| apply_in(unit, s));
            if outcome.is_err() || *throws { unit.rollback_to(depth) } else { unit.release(depth) }
        }
        write => {
            let op = op_of_unit(unit, write);
            unit.write(&super::batch(vec![op])).map(|_| ())
        }
    }
}

/// Applies a step at the top
fn run(db: &Database, step: &Step) -> Result<(), crate::Error> {
    match step {
        Step::RefuseNext => {
            crate::failpoints::fail_once_at(crate::failpoints::points::COMMIT, db.path());
            Ok(())
        }
        Step::Unit { steps, throws, durable } => {
            let mut unit = db.begin()?;
            let outcome = steps.iter().try_for_each(|s| apply_in(&mut unit, s));
            if let Err(e) = outcome {
                unit.abort();
                return Err(e);
            }
            if *throws {
                unit.abort();
                return Err(crate::Error::new(crate::ErrorCode::Io, "the unit threw"));
            }
            unit.commit(*durable).map(|_| ())
        }
        write => db.write(&super::batch(vec![op_of(db, write)]), false).map(|_| ()),
    }
}

/// The data as the model compares it: each entity's fields, whether it is trashed and its roles, and every edge
type State = (BTreeMap<String, (BTreeMap<String, Json>, bool, Vec<String>)>, Vec<(String, String, String)>);

/// The engine's data, as the model holds it
fn state_of(db: &Database) -> State {
    let mut entities = BTreeMap::new();
    for id in IDS {
        let Some(Json::Object(row)) = row(db, id) else { continue };
        let trashed = row.contains_key("trashedAt");
        let roles = row
            .get("role")
            .and_then(Json::as_array)
            .map(|r| r.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default();
        let fields = row.into_iter().filter(|(k, _)| !ENGINE_FIELDS.contains(&k.as_str())).collect();
        entities.insert(id.to_owned(), (fields, trashed, roles));
    }
    let edges = stored_edges(db)
        .into_iter()
        .map(|e| {
            let mut parts = e.split('|').map(str::to_owned);
            (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap())
        })
        .collect();
    (entities, edges)
}

fn model_state(m: &Model) -> State {
    let entities =
        m.entities.iter().map(|(id, e)| (id.clone(), (e.fields.clone(), e.trashed, e.roles.clone()))).collect();
    let edges = m.edges.iter().flat_map(|(edge, n)| std::iter::repeat_n(edge.clone(), *n)).collect();
    (entities, edges)
}

/// The ids under a derived key range, in key order
fn ids_under(s: &impl Read, prefix: &[u8]) -> Vec<String> {
    s.scan(prefix).unwrap().into_iter().map(|(_, v)| values::decode(&v).unwrap().as_str().unwrap().to_owned()).collect()
}

/// Every derived read, as the engine's keys answer it
fn derived_of(db: &Database) -> BTreeMap<String, Json> {
    db.snapshot(|s| {
        let mut out = BTreeMap::new();
        for t in ["Task", "Note"] {
            out.insert(format!("{t} live"), json!(ids_under(s, &keys::type_range(false, t))));
            out.insert(format!("{t} trashed"), json!(ids_under(s, &keys::type_range(true, t))));
            out.insert(format!("{t} count"), json!(entities::count(s, t).unwrap()));
            for role in ROLES {
                out.insert(
                    format!("{t} role {role}"),
                    json!(ids_under(s, &keys::index_range(t, "role", &[Part::from(role)]))),
                );
            }
        }
        // Task's index on `a`: every entry, as (part, id)
        let mut entries = Vec::new();
        for (k, v) in s.scan(&keys::index_range("Task", "a", &[])).unwrap() {
            let parts = decode_key(&k).unwrap();
            entries.push(json!([format!("{:?}", parts[3]), values::decode(&v).unwrap().as_str().unwrap()]));
        }
        out.insert("Task index a".to_owned(), json!(entries));
        for id in IDS {
            for kind in KINDS {
                let far = |outward| {
                    relations::edges(s, id, outward, Some(kind))
                        .unwrap()
                        .into_iter()
                        .map(|(_, f)| f)
                        .collect::<Vec<_>>()
                };
                out.insert(format!("{id} out {kind}"), json!(far(true)));
                out.insert(format!("{id} in {kind}"), json!(far(false)));
            }
        }
        Ok(out)
    })
    .unwrap()
}

/// What the derived reads should answer, from the model
fn derived_from(m: &Model) -> BTreeMap<String, Json> {
    let mut out = BTreeMap::new();
    let mut by_seq: Vec<(&String, &Entity)> = m.entities.iter().collect();
    by_seq.sort_by_key(|(_, e)| e.seq);
    for t in ["Task", "Note"] {
        let of_type = || by_seq.iter().filter(move |(id, _)| crate::engine::ids::type_of(id) == t);
        out.insert(
            format!("{t} live"),
            json!(of_type().filter(|(_, e)| !e.trashed).map(|(id, _)| id).collect::<Vec<_>>()),
        );
        out.insert(
            format!("{t} trashed"),
            json!(of_type().filter(|(_, e)| e.trashed).map(|(id, _)| id).collect::<Vec<_>>()),
        );
        out.insert(format!("{t} count"), json!(of_type().filter(|(_, e)| !e.trashed).count()));
        for role in ROLES {
            let holders: Vec<&String> =
                of_type().filter(|(_, e)| e.roles.iter().any(|r| r == role)).map(|(id, _)| *id).collect();
            out.insert(format!("{t} role {role}"), json!(holders));
        }
    }
    // The index's entries sort by part, then by place: rebuild that order from the model
    let mut entries: Vec<(Vec<u8>, u64, String, String)> = Vec::new();
    for (id, e) in by_seq.iter().filter(|(id, _)| crate::engine::ids::type_of(id) == "Task") {
        let value = e.fields.get("a").map(|v| serde_json::from_value::<rmpv::Value>(v.clone()).unwrap());
        for part in values::index_parts(value.as_ref()) {
            entries.push((
                crate::format::key::encode(std::slice::from_ref(&part)),
                e.seq,
                format!("{part:?}"),
                (*id).clone(),
            ));
        }
    }
    entries.sort();
    out.insert(
        "Task index a".to_owned(),
        json!(entries.into_iter().map(|(_, _, p, id)| json!([p, id])).collect::<Vec<_>>()),
    );
    // Far ends in the order linked: the model links in step order, which is each edge's place; recompute from the
    // engine's own order isn't the model's job, so compare as sets of far ends in a stable order
    for id in IDS {
        for kind in KINDS {
            let all = || m.edges.iter().flat_map(|(edge, n)| std::iter::repeat_n(edge, *n));
            let mut outs: Vec<&String> = all().filter(|(s, k, _)| s == id && k == kind).map(|(_, _, t)| t).collect();
            let mut ins: Vec<&String> = all().filter(|(_, k, t)| t == id && k == kind).map(|(s, _, _)| s).collect();
            outs.sort();
            ins.sort();
            out.insert(format!("{id} out {kind}"), json!(outs));
            out.insert(format!("{id} in {kind}"), json!(ins));
        }
    }
    out
}

/// The engine's far ends sorted, since the model doesn't keep link order
fn sorted_far_ends(mut derived: BTreeMap<String, Json>) -> BTreeMap<String, Json> {
    for (key, value) in derived.iter_mut() {
        if (key.contains(" out ") || key.contains(" in "))
            && let Json::Array(items) = value
        {
            items.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        }
    }
    derived
}

/// The words a search of Note's text index (its fields `a` and `b`) asks for: strings the workload writes
const SEARCHED: [&str; 4] = ["a", "ab", "abc", "c"];

/// The substring and pattern searches of Note's trigram index (its fields `a` and `b`): `containing` with a text and
/// whether it ignores case, or `matching` with a pattern. Short texts narrow by nothing, which walks the type
const GREPS: [(&str, &str, bool); 9] = [
    ("containing", "abc", false),
    ("containing", "ab", false),
    ("containing", "BCA", true),
    ("containing", "BCA", false),
    ("containing", "5555", false),
    ("matching", "a.c", false),
    ("matching", "^(ab|ca)c", false),
    ("matching", "c{3}|bab", false),
    ("matching", "[0-9]{4}", false),
];

fn grep_step(n: usize) -> Json {
    match GREPS[n] {
        ("containing", text, case_insensitive) => {
            json!({ "step": "containing", "text": text, "caseInsensitive": case_insensitive })
        }
        (_, pattern, _) => json!({ "step": "matching", "pattern": pattern }),
    }
}

/// Whether a scan of the fields finds what grep `n` asks: a string, or a list's strings
fn grepped(fields: &BTreeMap<String, Json>, n: usize) -> bool {
    let (kind, text, case_insensitive) = GREPS[n];
    let pattern = regex::Regex::new(text).unwrap();
    let found = |s: &str| match (kind, case_insensitive) {
        ("containing", false) => s.contains(text),
        ("containing", true) => s.to_lowercase().contains(&text.to_lowercase()),
        _ => pattern.is_match(s),
    };
    ["a", "b"].iter().any(|f| match fields.get(*f) {
        Some(Json::String(s)) => found(s),
        Some(Json::Array(items)) => items.iter().filter_map(Json::as_str).any(found),
        _ => false,
    })
}

/// What a `where` asks for: every number the workload writes, alone or in a list, and two of its long strings
fn probes() -> Vec<Json> {
    let mut out: Vec<Json> = (0..10).map(Json::from).collect();
    out.push(Json::from("0".repeat(2100)));
    out.push(Json::from("5".repeat(2100)));
    out
}

/// The derived reads as data: a label and its read, a query plan answering ids or a count
fn derived_reads() -> Vec<(String, Json)> {
    let mut out = Vec::new();
    let ids = |start: Json, steps: Json, scope: &str| json!({ "read": "query", "plan": { "scope": scope, "start": start, "steps": steps, "terminal": { "answer": "ids" } } });
    for t in ["Task", "Note"] {
        let start = json!({ "from": "types", "types": [t] });
        out.push((format!("{t} live"), ids(start.clone(), json!([]), "live")));
        out.push((format!("{t} trashed"), ids(start.clone(), json!([]), "trashed")));
        out.push((format!("{t} with trashed"), ids(start.clone(), json!([]), "withTrashed")));
        out.push((
            format!("{t} count"),
            json!({ "read": "query", "plan": { "start": start, "terminal": { "answer": "count" } } }),
        ));
        for probe in probes() {
            for field in ["a", "b"] {
                let label = format!(
                    "{t} where {field} {}",
                    if probe.is_string() {
                        format!("long {}", &probe.as_str().unwrap()[..1])
                    } else {
                        probe.to_string()
                    }
                );
                out.push((
                    label,
                    ids(start.clone(), json!([{ "step": "where", "field": field, "value": probe }]), "live"),
                ));
            }
        }
        if t == "Note" {
            for n in 0..GREPS.len() {
                out.push((format!("{t} grep {n}"), ids(start.clone(), json!([grep_step(n)]), "live")));
                out.push((
                    format!("{t} grep {n} with trashed"),
                    ids(start.clone(), json!([grep_step(n)]), "withTrashed"),
                ));
                // Among a set of both types: Task has no trigram index, so none of its entities matches
                out.push((
                    format!("{t} grep {n} among"),
                    ids(json!({ "from": "ids", "ids": IDS }), json!([grep_step(n)]), "live"),
                ));
            }
            for word in SEARCHED {
                let steps = json!([{ "step": "search", "query": word, "options": { "fuzzy": false } }]);
                out.push((format!("{t} search {word}"), ids(start.clone(), steps, "live")));
            }
        }
        for role in ROLES {
            out.push((
                format!("{t} role {role}"),
                ids(start.clone(), json!([{ "step": "withRole", "role": role }]), "live"),
            ));
        }
    }
    for id in IDS {
        for kind in KINDS {
            for step in ["out", "in"] {
                out.push((
                    format!("{id} {step} {kind}"),
                    ids(json!({ "from": "ids", "ids": [id] }), json!([{ "step": step, "kinds": [kind] }]), "live"),
                ));
            }
        }
    }
    out
}

/// The engine's answers to every derived read, in one call; far ends sorted, since the model doesn't keep link order
fn plans_of(db: &Database) -> BTreeMap<String, Json> {
    let reads = derived_reads();
    let request = crate::engine::plan::Reads {
        reads: reads.iter().map(|(_, r)| serde_json::from_value(r.clone()).unwrap()).collect(),
        track: None,
    };
    let answers: Json =
        serde_json::to_value(rmpv::decode::read_value(&mut &db.read(&request).unwrap()[..]).unwrap()).unwrap();
    reads
        .into_iter()
        .zip(answers.as_array().unwrap())
        .map(|((label, _), answer)| {
            let mut value = if answer["kind"] == "count" { answer["n"].clone() } else { answer["ids"].clone() };
            if (label.contains(" out ") || label.contains(" in ") || label.contains(" search "))
                && let Json::Array(items) = &mut value
            {
                items.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            }
            (label, value)
        })
        .collect()
}

/// What the derived reads should answer, from the model
fn plans_from(m: &Model) -> BTreeMap<String, Json> {
    let mut by_seq: Vec<(&String, &Entity)> = m.entities.iter().collect();
    by_seq.sort_by_key(|(_, e)| e.seq);
    let live = |id: &str| m.entities.get(id).is_some_and(|e| !e.trashed);
    let holds = |stored: Option<&Json>, probe: &Json| match stored {
        None => false,
        Some(Json::Array(items)) => items.iter().any(|v| v == probe),
        Some(v) => v == probe,
    };
    let mut out = BTreeMap::new();
    for (label, _) in derived_reads() {
        let words: Vec<&str> = label.split(' ').collect();
        let value = if IDS.contains(&words[0]) {
            let (id, step, kind) = (words[0], words[1], words[2]);
            let mut far: Vec<&String> = if !live(id) {
                vec![]
            } else {
                let all = m.edges.keys();
                let mut ends: Vec<&String> = if step == "out" {
                    all.filter(|(s, k, t)| s == id && k == kind && t != id).map(|(_, _, t)| t).collect()
                } else {
                    all.filter(|(s, k, t)| t == id && k == kind && s != id).map(|(s, _, _)| s).collect()
                };
                ends.retain(|e| live(e));
                ends
            };
            far.sort();
            far.dedup();
            json!(far)
        } else {
            let t = words[0];
            let of_type = || by_seq.iter().filter(move |(id, _)| crate::engine::ids::type_of(id) == t);
            match &words[1..] {
                ["live"] => json!(of_type().filter(|(_, e)| !e.trashed).map(|(id, _)| id).collect::<Vec<_>>()),
                ["trashed"] => json!(of_type().filter(|(_, e)| e.trashed).map(|(id, _)| id).collect::<Vec<_>>()),
                ["with", "trashed"] => json!(of_type().map(|(id, _)| id).collect::<Vec<_>>()),
                ["count"] => json!(of_type().filter(|(_, e)| !e.trashed).count()),
                // A field's text is a string or a list's strings; the workload's strings are one word each. Ranking
                // is the corpora specs' (`text/tests.rs`): a match set is ordered here as the engine's ties are
                ["search", word] => {
                    let mut found: Vec<&String> = of_type()
                        .filter(|(_, e)| {
                            !e.trashed
                                && ["a", "b"].iter().any(|f| match e.fields.get(*f) {
                                    Some(Json::String(s)) => s == word,
                                    Some(Json::Array(items)) => items.iter().any(|v| v.as_str() == Some(*word)),
                                    _ => false,
                                })
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    found.sort();
                    json!(found)
                }
                ["grep", n, rest @ ..] => {
                    let n: usize = n.parse().unwrap();
                    match rest {
                        [] => json!(
                            of_type()
                                .filter(|(_, e)| !e.trashed && grepped(&e.fields, n))
                                .map(|(id, _)| id)
                                .collect::<Vec<_>>()
                        ),
                        ["with", "trashed"] => {
                            json!(
                                of_type().filter(|(_, e)| grepped(&e.fields, n)).map(|(id, _)| id).collect::<Vec<_>>()
                            )
                        }
                        ["among"] => json!(
                            IDS.iter()
                                .filter(|id| crate::engine::ids::type_of(id) == t)
                                .filter(|id| m.entities.get(**id).is_some_and(|e| !e.trashed && grepped(&e.fields, n)))
                                .collect::<Vec<_>>()
                        ),
                        other => unreachable!("{other:?}"),
                    }
                }
                ["role", role] => json!(
                    of_type()
                        .filter(|(_, e)| !e.trashed && e.roles.iter().any(|r| r == role))
                        .map(|(id, _)| id)
                        .collect::<Vec<_>>()
                ),
                ["where", field, rest @ ..] => {
                    let probe = match rest {
                        ["long", digit] => Json::from(digit.repeat(2100)),
                        [n] => Json::from(n.parse::<i64>().unwrap()),
                        _ => unreachable!(),
                    };
                    json!(
                        of_type()
                            .filter(|(_, e)| !e.trashed && holds(e.fields.get(*field), &probe))
                            .map(|(id, _)| id)
                            .collect::<Vec<_>>()
                    )
                }
                other => unreachable!("{other:?}"),
            }
        };
        out.insert(label, value);
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn the_engine_holds_what_the_model_holds_after_every_step(steps in workload()) {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let mut model = Model::default();
        for (i, step) in steps.iter().enumerate() {
            let expected = model.step(step);
            let got = run(&db, step);
            prop_assert_eq!(got.is_ok(), expected.is_ok(), "step {}: {:?}: the engine said {:?}, the model {:?}", i, step, got, expected);
            prop_assert_eq!(state_of(&db), model_state(&model), "step {}: {:?}", i, step);
            prop_assert_eq!(sorted_far_ends(derived_of(&db)), derived_from(&model), "step {}: {:?}", i, step);
            prop_assert_eq!(plans_of(&db), plans_from(&model), "step {}: {:?}", i, step);
        }
        crate::failpoints::disarm(crate::failpoints::points::COMMIT);
        let report = db.check().unwrap();
        prop_assert!(report.is_clean(), "check() after the workload: {:?}", report);
    }
}

/// The crash harness (`docs/architecture/testing.md`): a child process runs a workload over a database, acknowledging each step as it
/// commits, and is killed with `SIGKILL` at a random point; the reopened files must hold the model's state after the
/// acknowledged steps, or after one more, the step in flight having committed before its acknowledgement. The child
/// regenerates the workload from its seed, so nothing but the seed crosses.
mod crash {
    use std::io::{BufRead, BufReader, Write as _};
    use std::process::{Command, Stdio};

    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

    use super::*;

    /// The seed's workload, with no refused commit, which only a backend in this process can inject
    fn seeded(seed: u64) -> Vec<Step> {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        let mut runner = TestRunner::new_with_rng(Config::default(), TestRng::from_seed(RngAlgorithm::ChaCha, &bytes));
        let steps = workload().new_tree(&mut runner).unwrap().current();
        steps.into_iter().filter(|s| !matches!(s, Step::RefuseNext)).collect()
    }

    /// What differs between two states: each entity that differs and each edge one has and the other doesn't
    fn differences(a: &State, b: &State) -> String {
        let mut out = Vec::new();
        for id in a.0.keys().chain(b.0.keys()).collect::<std::collections::BTreeSet<_>>() {
            if a.0.get(id) != b.0.get(id) {
                out.push(format!("{id}: {:?} / {:?}", a.0.get(id), b.0.get(id)));
            }
        }
        for e in a.1.iter().filter(|e| !b.1.contains(e)) {
            out.push(format!("edge only in the files: {e:?}"));
        }
        for e in b.1.iter().filter(|e| !a.1.contains(e)) {
            out.push(format!("edge only in the model: {e:?}"));
        }
        out.join("; ")
    }

    /// The writer half, run as a child of this test binary
    #[test]
    fn child_runs_a_seeded_workload_acknowledging_each_step() {
        let (Ok(path), Ok(seed)) = (std::env::var("ELMDB_CRASH_DB"), std::env::var("ELMDB_CRASH_SEED")) else { return };
        // Syncs on, as the app runs: a commit survives the process's death before any sync
        let db =
            Database::open(&path, crate::OpenOptions { schema: Some(super::super::schema()), ..Default::default() })
                .unwrap();
        let mut out = std::io::stdout().lock();
        for (i, step) in seeded(seed.parse().unwrap()).iter().enumerate() {
            let _ = run(&db, step);
            writeln!(out, "ack {i}").unwrap();
            out.flush().unwrap();
        }
        writeln!(out, "done").unwrap();
        out.flush().unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    #[test]
    fn a_killed_writer_leaves_the_state_after_its_acknowledged_steps_or_one_more() {
        for seed in 0..48u64 {
            let steps = seeded(seed);
            let dir = tempfile::tempdir().unwrap();
            // Somewhere past the prelude, and sometimes the whole workload
            let kill_at = 6 + (seed as usize * 7) % steps.len().max(7);
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "engine::tests::model::crash::child_runs_a_seeded_workload_acknowledging_each_step",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("ELMDB_CRASH_DB", dir.path())
                .env("ELMDB_CRASH_SEED", seed.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut acked = 0usize;
            let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
            for line in lines.by_ref() {
                let line = line.unwrap();
                if let Some(i) = line.strip_prefix("ack ") {
                    acked = i.parse::<usize>().unwrap() + 1;
                    if acked >= kill_at {
                        break;
                    }
                } else if line == "done" {
                    break;
                }
            }
            child.kill().unwrap();
            child.wait().unwrap();
            // The child went on between the acknowledgement read and the kill: every step it acknowledged is in the pipe
            for line in lines {
                let Ok(line) = line else { break };
                if let Some(i) = line.strip_prefix("ack ") {
                    acked = i.parse::<usize>().unwrap() + 1;
                }
            }
            let db = Database::open(dir.path(), crate::OpenOptions { syncs: false, ..Default::default() }).unwrap();
            let mut model = Model::default();
            for step in &steps[..acked] {
                let _ = model.step(step);
            }
            let after_acked = model_state(&model);
            if let Some(next) = steps.get(acked) {
                let _ = model.step(next);
            }
            let after_next = model_state(&model);
            let got = state_of(&db);
            assert!(
                got == after_acked || got == after_next,
                "seed {seed}: killed after {acked} acknowledged steps, the files hold neither the state after them nor after one more\n\
                 differing from the acknowledged state: {}\nthe step in flight: {:?}",
                differences(&got, &after_acked),
                steps.get(acked)
            );
            let report = db.check().unwrap();
            assert!(report.is_clean(), "seed {seed}: {report:?}");
        }
    }
}
