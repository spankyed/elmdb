//! The model harness (`docs/architecture/testing.md`), ported from apack's `tests/_harness/`: random writes over a small pool of ids,
//! so they collide, refer to each other and are refused, checked after every operation against a reference model kept
//! as plainly as a model can be. The engine must hold what the model holds, refuse what it refuses, and keep every
//! derived key in step: a type's live and trashed entities in creation order, its count, each value index's entries,
//! the role index, each entity's edges out and in, and what a text index matches, whether maintained by writes or
//! derived again by a rebuild; and a rank index over the entities and their edges holding the model's live entities,
//! within the bound it reports of `rank` once its drain is done.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use serde_json::{Value as Json, json};

use super::rank_bound::RankIndex;
use super::{edges as stored_edges, row, schema_json};
use crate::ChangeSet;
use crate::engine::unit::Read;
use crate::engine::{entities, keys, relations, values};
use crate::format::key::{Part, decode as decode_key};
use crate::schema::Schema;
use crate::{Database, OpenOptions};

const IDS: [&str; 6] = ["Task-1", "Task-2", "Task-3", "Task-4", "Note-1", "Note-2"];
const KEYS: [&str; 3] = ["a", "b", "c"];
const KINDS: [&str; 2] = ["refs", "blocks"];
/// The keyed kind: `cites`, keyed by its info's `n`, which the model's edges of it carry from a pool of two
const KEYED: &str = "cites";
/// Every kind the derived reads compare
const ALL_KINDS: [&str; 3] = ["refs", "blocks", KEYED];
const ROLES: [&str; 2] = ["r1", "r2"];
/// Every index rebuilt, or one of each kind
const REBUILDS: [Option<(&str, &str)>; 4] =
    [None, Some(("Task", "a")), Some(("Note", "text")), Some(("Note", "grams"))];
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
    /// Links by `cites` with the key `n`: the edge holding it, or a new one beside the pair's others
    LinkKeyed {
        id: &'static str,
        target: &'static str,
        n: u8,
    },
    /// Changes the key of the `cites` edge holding `from` to `to`, by its id; refused when there is none, or when
    /// another edge of the pair holds `to`
    Rekey {
        id: &'static str,
        target: &'static str,
        from: u8,
        to: u8,
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
    /// Moves the first relation between the ends to another target, by its id; refused when there is none, or when the
    /// new ends are joined already
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
    /// Derived data deleted and derived again from primary data, every index's or one's: the reconstruction `check()`
    /// shares, so only the model's answers can tell it wrong
    Rebuild {
        index: Option<(&'static str, &'static str)>,
    },
    /// Every entity, edge and derived row removed; the schema, the meta data and the sequence counter are kept
    Reset,
    /// Every edge of one kind moved to another: a commit with no change set, which stamps the ends of each edge it moved
    RenameRelation {
        from: &'static str,
        to: &'static str,
    },
    /// A `Task` field's values moved to another: the harness declares `spare` as a second unindexed field of `Task`
    RenameField {
        from: &'static str,
        to: &'static str,
    },
}

/// The field renames the workload makes, both ways between `c` and the spare field
const FIELD_RENAMES: [(&str, &str); 2] = [("c", "spare"), ("spare", "c")];

/// The kinds a relation rename moves between: the workload's two plain kinds and the one it never links
const RENAMES: [(&str, &str); 6] = [
    ("refs", "tags"),
    ("tags", "refs"),
    ("blocks", "tags"),
    ("tags", "blocks"),
    ("refs", "blocks"),
    ("blocks", "refs"),
];

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
        2 => (id(), id(), 0u8..2).prop_map(|(id, target, n)| Step::LinkKeyed { id, target, n }),
        1 => (id(), id(), 0u8..2, 0u8..2).prop_map(|(id, target, from, to)| Step::Rekey { id, target, from, to }),
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

/// A step and whether it was chosen from the model's state to succeed. Shrinking drops an aimed step the model would
/// refuse once what it was aimed at is gone (`preconditions`), so a shrunk case keeps steps that do what they were
/// chosen for; an arbitrary one is kept whatever it meets, so refusals are still compared
#[derive(Debug, Clone)]
struct Aimed {
    step: Step,
    aimed: bool,
}

/// The model as the state machine carries it: the model, the prelude that made it, and what the last step answered,
/// which the engine's answer must match
#[derive(Debug, Clone)]
struct Reference {
    model: Model,
    prelude: Vec<Step>,
    outcome: Result<(), Refusal>,
}

/// The workload as a state machine (proptest-state-machine): every id created first, with fields of its own, then
/// steps chosen from what the model holds, and one in eight chosen blind
struct ModelMachine;

impl proptest_state_machine::ReferenceStateMachine for ModelMachine {
    type State = Reference;
    type Transition = Aimed;

    fn init_state() -> BoxedStrategy<Reference> {
        prop::collection::vec(fields(), IDS.len())
            .prop_map(|all| {
                let prelude: Vec<Step> = IDS.iter().zip(all).map(|(id, fields)| Step::Create { id, fields }).collect();
                let mut model = Model::default();
                for step in &prelude {
                    model.step(step).expect("the prelude creates each id once");
                }
                Reference { model, prelude, outcome: Ok(()) }
            })
            .boxed()
    }

    fn transitions(state: &Reference) -> BoxedStrategy<Aimed> {
        let aimed = |steps: BoxedStrategy<Step>| steps.prop_map(|step| Aimed { step, aimed: true });
        let blind = |steps: BoxedStrategy<Step>| steps.prop_map(|step| Aimed { step, aimed: false });
        let rebuild = prop::sample::select(&REBUILDS[..]).prop_map(|index| Step::Rebuild { index }).boxed();
        let reset = Just(Step::Reset).boxed();
        let rename =
            prop::sample::select(&RENAMES[..]).prop_map(|(from, to)| Step::RenameRelation { from, to }).boxed();
        let field_rename =
            prop::sample::select(&FIELD_RENAMES[..]).prop_map(|(from, to)| Step::RenameField { from, to }).boxed();
        prop_oneof![
            7 => aimed(aimed_write(&state.model)),
            1 => blind(write()),
            2 => aimed(aimed_unit(&state.model, true)),
            1 => blind(Just(Step::RefuseNext).boxed()),
            1 => blind(rebuild),
            1 => blind(reset),
            1 => blind(rename),
            1 => blind(field_rename),
            1 => blind(onto_joined_ends(&state.model)),
        ]
        .boxed()
    }

    fn preconditions(state: &Reference, transition: &Aimed) -> bool {
        !transition.aimed
            || matches!(
                state.model.clone().step(&transition.step),
                Ok(()) | Err("the unit threw" | "the store refused the commit")
            )
    }

    fn apply(mut state: Reference, transition: &Aimed) -> Reference {
        state.outcome = state.model.step(&transition.step);
        state
    }
}

/// A move of an edge onto ends another edge of its kind already joins, or a `cites` edge's key changed to the one the
/// other edge of its pair holds, which the engine refuses and random steps rarely make; blind, since an aimed step must
/// be one the model takes. Any write when the model holds neither
fn onto_joined_ends(m: &Model) -> BoxedStrategy<Step> {
    let edges: Vec<(&'static str, &'static str, &'static str)> = m
        .edges
        .iter()
        .filter(|(_, k, _)| KINDS.contains(&k.as_str()))
        .map(|(s, k, t)| (named(&IDS, s), named(&KINDS, k), named(&IDS, t)))
        .collect();
    let onto: Vec<_> = edges
        .iter()
        .flat_map(|a| edges.iter().filter(move |b| a.0 == b.0 && a.1 == b.1 && a.2 != b.2).map(move |b| (*a, b.2)))
        .collect();
    // And a `cites` edge's key changed to the one the other edge of its pair holds
    let rekeys: Vec<_> = m
        .keyed
        .iter()
        .filter(|(s, t, n)| m.keyed.contains(&(s.clone(), t.clone(), 1 - n)))
        .map(|(s, t, n)| Step::Rekey { id: named(&IDS, s), target: named(&IDS, t), from: *n, to: 1 - n })
        .collect();
    let mut options: Vec<(u32, BoxedStrategy<Step>)> = Vec::new();
    if !onto.is_empty() {
        let moved = prop::sample::select(onto);
        options
            .push((1, moved.prop_map(|((id, kind, target), to)| Step::MoveRelation { id, kind, target, to }).boxed()));
    }
    if !rekeys.is_empty() {
        options.push((1, prop::sample::select(rekeys).boxed()));
    }
    if options.is_empty() {
        return write();
    }
    prop::strategy::Union::new_weighted(options).boxed()
}

/// The pool entry a model's string names, so a step built from the model holds the same `&'static str` a blind one does
fn named(pool: &[&'static str], name: &str) -> &'static str {
    pool.iter().copied().find(|p| *p == name).expect("the model names only the pool's")
}

/// One write the model would take, chosen from what it holds: an id it has, or lacks for a create; an edge it has; a
/// key holding a list, or none, for a list's operations
fn aimed_write(m: &Model) -> BoxedStrategy<Step> {
    let ids = |keep: &dyn Fn(Option<&Entity>) -> bool| -> Vec<&'static str> {
        IDS.iter().copied().filter(|id| keep(m.entities.get(*id))).collect()
    };
    let existing = ids(&|e| e.is_some());
    let missing = ids(&|e| e.is_none());
    let trashed = ids(&|e| e.is_some_and(|e| e.trashed));
    let edges: Vec<(&'static str, &'static str, &'static str)> = m
        .edges
        .iter()
        .filter(|(_, k, _)| KINDS.contains(&k.as_str()))
        .map(|(s, k, t)| (named(&IDS, s), named(&KINDS, k), named(&IDS, t)))
        .collect();
    let keys_where = |keep: &dyn Fn(Option<&Json>) -> bool| -> Vec<(&'static str, &'static str)> {
        existing
            .iter()
            .flat_map(|id| KEYS.iter().map(move |key| (*id, *key)))
            .filter(|(id, key)| keep(m.entities[*id].fields.get(*key)))
            .collect()
    };
    let listable = keys_where(&|v| matches!(v, None | Some(Json::Array(_))));
    let lists = keys_where(&|v| matches!(v, Some(Json::Array(_))));
    let removable = if lists.is_empty() { listable.clone() } else { lists };

    let mut options: Vec<(u32, BoxedStrategy<Step>)> = vec![
        (1, (id(), fields()).prop_map(|(id, fields)| Step::Upsert { id, fields }).boxed()),
        (1, id().prop_map(|id| Step::TakeLabel { id }).boxed()),
    ];
    if !missing.is_empty() {
        let id = prop::sample::select(missing);
        options.push((3, (id, fields()).prop_map(|(id, fields)| Step::Create { id, fields }).boxed()));
    }
    if !existing.is_empty() {
        let id = || prop::sample::select(existing.clone());
        options.extend([
            (3, (id(), key(), value()).prop_map(|(id, key, value)| Step::Set { id, key, value }).boxed()),
            (1, (id(), key()).prop_map(|(id, key)| Step::Delete { id, key }).boxed()),
            (1, id().prop_map(|id| Step::Destroy { id }).boxed()),
            (1, id().prop_map(|id| Step::Trash { id }).boxed()),
            (1, (id(), role()).prop_map(|(id, role)| Step::Grant { id, role }).boxed()),
            (1, (id(), role()).prop_map(|(id, role)| Step::Revoke { id, role }).boxed()),
            (1, (id(), role()).prop_map(|(id, role)| Step::GrantExclusive { id, role }).boxed()),
            (1, (id(), key(), value()).prop_map(|(id, key, value)| Step::Merge { id, key, value }).boxed()),
        ]);
    }
    if !listable.is_empty() {
        let at = prop::sample::select(listable);
        options.push((2, (at, 0i64..10).prop_map(|((id, key), value)| Step::Append { id, key, value }).boxed()));
    }
    if !removable.is_empty() {
        let at = || prop::sample::select(removable.clone());
        let member = prop_oneof![
            (0i64..10).prop_map(Json::from),
            prop::collection::btree_map("[k-m]", 0i64..3, 1..2).prop_map(|m| json!(m)),
        ];
        options.extend([
            (1, (at(), 0u32..3).prop_map(|((id, key), index)| Step::RemoveAt { id, key, index }).boxed()),
            (1, (at(), member).prop_map(|((id, key), value)| Step::RemoveValue { id, key, value }).boxed()),
        ]);
    }
    let pairs: Vec<(&'static str, &'static str)> =
        existing.iter().flat_map(|s| existing.iter().filter(move |t| *t != s).map(move |t| (*s, *t))).collect();
    if !pairs.is_empty() {
        let pair = prop::sample::select(pairs.clone());
        options.push((3, (pair, kind()).prop_map(|((id, target), kind)| Step::Link { id, kind, target }).boxed()));
        let pair = prop::sample::select(pairs.clone());
        options.push((2, (pair, 0u8..2).prop_map(|((id, target), n)| Step::LinkKeyed { id, target, n }).boxed()));
    }
    // A `cites` edge onto the key its pair doesn't hold, or onto its own
    let rekeys: Vec<_> = m
        .keyed
        .iter()
        .flat_map(|(s, t, from)| {
            (0u8..2)
                .filter(move |to| to == from || !m.keyed.contains(&(s.clone(), t.clone(), *to)))
                .map(move |to| (named(&IDS, s), named(&IDS, t), *from, to))
        })
        .collect();
    if !rekeys.is_empty() {
        let rekey = prop::sample::select(rekeys);
        options.push((1, rekey.prop_map(|(id, target, from, to)| Step::Rekey { id, target, from, to }).boxed()));
    }
    if !edges.is_empty() {
        let edge = || prop::sample::select(edges.clone());
        options.extend([
            (1, edge().prop_map(|(id, kind, target)| Step::Unlink { id, kind, target }).boxed()),
            (1, edge().prop_map(|(id, kind, target)| Step::RemoveRelation { id, kind, target }).boxed()),
        ]);
        let moves: Vec<_> = edges
            .iter()
            .flat_map(|edge| {
                existing.iter().filter(move |to| **to != edge.0 && **to != edge.2).map(move |to| (*edge, *to))
            })
            .collect();
        if !moves.is_empty() {
            let moved = prop::sample::select(moves);
            options.push((
                1,
                moved.prop_map(|((id, kind, target), to)| Step::MoveRelation { id, kind, target, to }).boxed(),
            ));
        }
    }
    if !trashed.is_empty() {
        options.push((1, prop::sample::select(trashed).prop_map(|id| Step::Restore { id }).boxed()));
    }
    prop::strategy::Union::new_weighted(options).boxed()
}

/// Writes made as one unit, each chosen from the model as the unit starts, one in four throwing; at the top, one of them
/// sometimes a unit of its own, a savepoint
fn aimed_unit(m: &Model, top: bool) -> BoxedStrategy<Step> {
    let inner = if top { prop_oneof![4 => aimed_write(m), 1 => aimed_unit(m, false)].boxed() } else { aimed_write(m) };
    (prop::collection::vec(inner, 1..6), prop::bool::weighted(0.25), any::<bool>())
        .prop_map(|(steps, throws, durable)| Step::Unit { steps, throws, durable })
        .boxed()
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
    /// Each edge by its ends: a kind with no key joins two entities by one edge at most
    edges: BTreeSet<(String, String, String)>,
    /// Each `cites` edge by its ends and key: one a pair for each value of `n`
    keyed: BTreeSet<(String, String, u8)>,
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
                if self.edges.insert(edge.clone()) {
                    self.pending.push(edge);
                }
            }
            Step::LinkKeyed { id, target, n } => {
                self.exists(id)?;
                if id == target {
                    return Err("self link");
                }
                if self.keyed.insert((id.to_string(), target.to_string(), *n)) {
                    self.pending.push((id.to_string(), KEYED.to_owned(), target.to_string()));
                }
            }
            Step::Rekey { id, target, from, to } => {
                let held = (id.to_string(), target.to_string(), *from);
                if !self.keyed.contains(&held) {
                    return Err("no such relation");
                }
                let rekeyed = (id.to_string(), target.to_string(), *to);
                if from != to && self.keyed.contains(&rekeyed) {
                    return Err("duplicate edge");
                }
                self.keyed.remove(&held);
                self.keyed.insert(rekeyed);
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
                if !self.edges.remove(&edge) {
                    return Err("no such relation");
                }
            }
            Step::MoveRelation { id, kind, target, to } => {
                let edge = (id.to_string(), kind.to_string(), target.to_string());
                if !self.edges.contains(&edge) {
                    return Err("no such relation");
                }
                if id == to {
                    return Err("self link");
                }
                let moved = (id.to_string(), kind.to_string(), to.to_string());
                if moved != edge && self.edges.contains(&moved) {
                    return Err("duplicate edge");
                }
                self.edges.remove(&edge);
                self.edges.insert(moved.clone());
                self.pending.push(moved);
            }
            Step::TakeLabel { .. } => {}
            Step::Destroy { id } => {
                self.exists(id)?;
                self.entities.remove(*id);
                self.edges.retain(|(s, _, t)| s != id && t != id);
                self.keyed.retain(|(s, t, _)| s != id && t != id);
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
            // What the model holds is primary data, which a rebuild leaves as it is
            Step::Rebuild { .. } => {}
            Step::RefuseNext | Step::Reset | Step::RenameRelation { .. } | Step::RenameField { .. } => {
                unreachable!("only at the top")
            }
        }
        Ok(())
    }

    /// The deferred check: every edge the unit linked that still exists has its target
    fn check_pending(&self) -> Result<(), Refusal> {
        for edge in &self.pending {
            let held = self.edges.contains(edge)
                || (edge.1 == KEYED && self.keyed.iter().any(|(s, t, _)| *s == edge.0 && *t == edge.2));
            if held && !self.entities.contains_key(&edge.2) {
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
        if let Step::RenameField { from, to } = step {
            // A Task holding both names would have one value replaced: refused before the commit, so an armed refusal stays
            let clash = self.entities.iter().any(|(id, e)| {
                crate::engine::ids::type_of(id) == "Task" && e.fields.contains_key(*from) && e.fields.contains_key(*to)
            });
            if clash {
                return Err("a Task holds both names");
            }
            if std::mem::take(&mut self.refuse_next) {
                return Err("the store refused the commit");
            }
            self.pending.clear();
            for (id, entity) in self.entities.iter_mut() {
                if crate::engine::ids::type_of(id) == "Task"
                    && let Some(value) = entity.fields.remove(*from)
                {
                    entity.fields.insert(to.to_string(), value);
                }
            }
            return Ok(());
        }
        if let Step::RenameRelation { from, to } = step {
            // An edge of the new kind joining the ends of a moving edge would be one identity twice: refused, nothing moves.
            // The engine refuses that before its commit, so an armed refusal stays armed for the next commit
            let moving: Vec<(String, String, String)> =
                self.edges.iter().filter(|(_, kind, _)| kind.as_str() == *from).cloned().collect();
            if moving
                .iter()
                .any(|(source, _, target)| self.edges.contains(&(source.clone(), to.to_string(), target.clone())))
            {
                return Err("an edge of the new kind joins the ends");
            }
            if std::mem::take(&mut self.refuse_next) {
                return Err("the store refused the commit");
            }
            self.pending.clear();
            for (source, kind, target) in moving {
                self.edges.remove(&(source.clone(), kind, target.clone()));
                self.edges.insert((source, to.to_string(), target));
            }
            return Ok(());
        }
        if let Step::Reset = step {
            if std::mem::take(&mut self.refuse_next) {
                return Err("the store refused the commit");
            }
            self.entities.clear();
            self.edges.clear();
            self.keyed.clear();
            self.pending.clear();
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

/// The `cites` relation between the ends holding the key `n`, or an id no relation has
fn keyed_between(r: &impl Read, source: &str, target: &str, n: u8) -> String {
    let key = rmpv::Value::Map(vec![("n".into(), n.into())]);
    relations::between(r, source, KEYED, target)
        .unwrap()
        .into_iter()
        .find(|relation| relations::get(r, relation).unwrap().is_some_and(|row| row.info.as_ref() == Some(&key)))
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
        Step::Rekey { id, target, from, to } => {
            let relation = db.snapshot(|s| Ok(keyed_between(s, id, target, *from))).unwrap();
            json!({ "op": "updateRelation", "relation": relation, "info": { "n": to } })
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
        Step::Rekey { id, target, from, to } => {
            let relation = unit.read_with(|s| Ok(keyed_between(s, id, target, *from))).unwrap();
            json!({ "op": "updateRelation", "relation": relation, "info": { "n": to } })
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
        Step::LinkKeyed { id, target, n } => {
            json!({ "op": "link", "source": id, "kind": KEYED, "target": target, "info": { "n": n } })
        }
        Step::Destroy { id } => json!({ "op": "destroy", "id": id }),
        Step::Trash { id } => json!({ "op": "trash", "id": id }),
        Step::Restore { id } => json!({ "op": "restore", "id": id }),
        Step::Grant { id, role } => json!({ "op": "grant", "id": id, "role": role }),
        Step::Revoke { id, role } => json!({ "op": "revoke", "id": id, "role": role }),
        Step::RemoveRelation { .. }
        | Step::MoveRelation { .. }
        | Step::Rekey { .. }
        | Step::Unit { .. }
        | Step::RefuseNext
        | Step::Reset
        | Step::RenameRelation { .. }
        | Step::RenameField { .. }
        | Step::Rebuild { .. } => {
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

/// Runs one step on the engine: the change set of the commit it made, or `None` when it made none
fn run(db: &Database, step: &Step) -> Result<Option<ChangeSet>, crate::Error> {
    match step {
        Step::RefuseNext => {
            crate::failpoints::fail_once_at(crate::failpoints::points::COMMIT, db.path());
            Ok(None)
        }
        Step::Rebuild { index } => db.rebuild(*index).map(|()| None),
        Step::Reset => db.reset().map(|()| None),
        Step::RenameRelation { from, to } => db.rename_relation(from, to).map(|()| None),
        Step::RenameField { from, to } => db.rename_field("Task", from, to).map(|()| None),
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
            unit.commit(*durable).map(Some)
        }
        write => db.write(&super::batch(vec![op_of(db, write)]), false).map(|(_, changes)| Some(changes)),
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
        .collect::<Vec<_>>();
    let mut edges = edges;
    edges.sort();
    (entities, edges)
}

fn model_state(m: &Model) -> State {
    let entities =
        m.entities.iter().map(|(id, e)| (id.clone(), (e.fields.clone(), e.trashed, e.roles.clone()))).collect();
    (entities, all_edges(m))
}

/// Every edge the model holds by its ends, a `cites` edge once for each key its pair holds, sorted
fn all_edges(m: &Model) -> Vec<(String, String, String)> {
    let mut edges: Vec<_> = m.edges.iter().cloned().collect();
    edges.extend(m.keyed.iter().map(|(s, t, _)| (s.clone(), KEYED.to_owned(), t.clone())));
    edges.sort();
    edges
}

/// The ids of the entities a value index files under a key
fn filed(s: &impl Read, entity_type: &str, own: &[u8]) -> Vec<String> {
    super::filed_under(s, entity_type, own).into_iter().map(|(_, id)| id).collect()
}

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
                    json!(filed(s, t, &keys::index_range(t, "role", &[Part::from(role)]))),
                );
            }
        }
        // Task's index on `a`: every entry, as (part, id)
        let mut entries = Vec::new();
        for (k, id) in super::filed_under(s, "Task", &keys::index_range("Task", "a", &[])) {
            let parts = decode_key(&k).unwrap();
            entries.push(json!([format!("{:?}", parts[3]), id]));
        }
        out.insert("Task index a".to_owned(), json!(entries));
        for kind in ALL_KINDS {
            out.insert(format!("{kind} relations"), json!(relations::count(s, kind).unwrap()));
        }
        for id in IDS {
            for kind in ALL_KINDS {
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
    for kind in ALL_KINDS {
        out.insert(format!("{kind} relations"), json!(all_edges(m).iter().filter(|e| e.1 == kind).count()));
    }
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
    let edges = all_edges(m);
    for id in IDS {
        for kind in ALL_KINDS {
            let all = || edges.iter();
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
        // Ordered, whole and its first one or three, the path that keeps only the first: and a filter's first two
        for field in ["a", "b"] {
            for descending in [false, true] {
                for limit in [None, Some(1), Some(3)] {
                    let mut steps = vec![json!({ "step": "orderBy", "field": field, "descending": descending })];
                    steps.extend(limit.map(|n| json!({ "step": "limit", "n": n })));
                    let label = format!(
                        "{t} order {field} {} {}",
                        if descending { "desc" } else { "asc" },
                        limit.map_or("all".to_owned(), |n| n.to_string())
                    );
                    out.push((label, ids(start.clone(), json!(steps), "live")));
                }
            }
            for probe in [3, 5] {
                let steps = json!([{ "step": "where", "field": field, "value": probe }, { "step": "limit", "n": 2 }]);
                out.push((format!("{t} first 2 where {field} {probe}"), ids(start.clone(), steps, "live")));
            }
            // A range of numbers, with both bounds and with one; and one put in order, its first two
            let between = json!({ "step": "range", "field": field, "atLeast": 2, "below": 6 });
            out.push((format!("{t} range {field} 2 6"), ids(start.clone(), json!([between]), "live")));
            let above = json!({ "step": "range", "field": field, "above": 4 });
            out.push((format!("{t} range {field} 4 up"), ids(start.clone(), json!([above]), "live")));
            for descending in [false, true] {
                let order = json!({ "step": "orderBy", "field": field, "descending": descending });
                let direction = if descending { "desc" } else { "asc" };
                let steps = json!([between, order, { "step": "limit", "n": 2 }]);
                out.push((format!("{t} ranged {field} {direction}"), ids(start.clone(), steps, "live")));
                // The first two in the field's order of those the other field's value keeps: a task's `a` under
                // its `b` is read from the index on the two, and the rest are sorted
                let other = if field == "a" { "b" } else { "a" };
                let kept = json!({ "step": "where", "field": other, "value": 3 });
                let steps = json!([kept, order, { "step": "limit", "n": 2 }]);
                out.push((format!("{t} filtered {field} {direction}"), ids(start.clone(), steps, "live")));
                // And of those, the ones in the range, whichever of the two is asked first, and a page after each id
                for (way, steps) in [("after", json!([kept, between])), ("before", json!([between, kept]))] {
                    out.push((format!("{t} filtered range {field} {way}"), ids(start.clone(), steps, "live")));
                }
                for cursor in IDS {
                    let steps = json!([kept, order, { "step": "after", "id": cursor }, { "step": "limit", "n": 2 }]);
                    let label = format!("{t} filtered page {field} {direction} {cursor}");
                    out.push((label, ids(start.clone(), steps, "live")));
                }
                // A page of two after each of the type's ids, and after one of the other type's, in the field's order
                for cursor in IDS {
                    let steps = json!([order, { "step": "after", "id": cursor }, { "step": "limit", "n": 2 }]);
                    out.push((format!("{t} page {field} {direction} {cursor}"), ids(start.clone(), steps, "live")));
                }
            }
        }
        // A page of two after each id in the order the type was created in, and among the holders of a value
        for cursor in IDS {
            let steps = json!([{ "step": "after", "id": cursor }, { "step": "limit", "n": 2 }]);
            out.push((format!("{t} after {cursor}"), ids(start.clone(), steps, "live")));
            let steps = json!([
                { "step": "where", "field": "a", "value": 3 },
                { "step": "after", "id": cursor },
                { "step": "limit", "n": 2 }
            ]);
            out.push((format!("{t} holders after {cursor}"), ids(start.clone(), steps, "live")));
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
            // A role's holders that hold a value in `a`, or one in a range of it, whichever of the two filters is
            // asked first: a task has an index for each, and the one holding the fewer entries is read
            let held = json!({ "step": "withRole", "role": role });
            let filters = [
                ("3", json!({ "step": "where", "field": "a", "value": 3 })),
                ("5", json!({ "step": "where", "field": "a", "value": 5 })),
                ("range", json!({ "step": "range", "field": "a", "atLeast": 2, "below": 6 })),
            ];
            for (asked, filter) in filters {
                for (way, steps) in [("after", json!([filter, held])), ("before", json!([held, filter]))] {
                    out.push((format!("{t} paired {way} {role} {asked}"), ids(start.clone(), steps, "live")));
                }
            }
        }
    }
    for id in IDS {
        for kind in ALL_KINDS {
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

/// How `orderBy` orders two values: booleans, numbers, strings by the root collation, a list or map by its JSON, then
/// a missing or null value
fn ordered_as(
    collator: &icu_collator::CollatorBorrowed<'static>,
    a: Option<&Json>,
    b: Option<&Json>,
) -> std::cmp::Ordering {
    let class = |v: Option<&Json>| match v {
        None | Some(Json::Null) => 4,
        Some(Json::Bool(_)) => 0,
        Some(Json::Number(_)) => 1,
        Some(Json::String(_)) => 2,
        Some(_) => 3,
    };
    class(a).cmp(&class(b)).then_with(|| match (a, b) {
        (Some(Json::Bool(x)), Some(Json::Bool(y))) => x.cmp(y),
        (Some(Json::Number(x)), Some(Json::Number(y))) => x.as_f64().partial_cmp(&y.as_f64()).unwrap(),
        (Some(Json::String(x)), Some(Json::String(y))) => collator.compare(x, y),
        (Some(x), Some(y)) if class(a) == 3 => collator.compare(&x.to_string(), &y.to_string()),
        _ => std::cmp::Ordering::Equal,
    })
}

/// Whether a `range` holds a value: a number `within` it, or a list holding one
fn in_range(value: Option<&Json>, within: &impl Fn(f64) -> bool) -> bool {
    match value {
        Some(Json::Array(items)) => items.iter().filter_map(Json::as_f64).any(within),
        Some(value) => value.as_f64().is_some_and(within),
        None => false,
    }
}

/// The two ids after `cursor` among `ids`, or none when it isn't one of them
fn page_after<'a>(ids: impl Iterator<Item = &'a String>, cursor: &str) -> Vec<&'a String> {
    ids.skip_while(|id| *id != cursor).skip(1).take(2).collect()
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
    let collator = icu_collator::CollatorBorrowed::try_new(Default::default(), Default::default()).unwrap();
    let mut out = BTreeMap::new();
    for (label, _) in derived_reads() {
        let words: Vec<&str> = label.split(' ').collect();
        let value = if IDS.contains(&words[0]) {
            let (id, step, kind) = (words[0], words[1], words[2]);
            let edges = all_edges(m);
            let mut far: Vec<&String> = if !live(id) {
                vec![]
            } else {
                let all = edges.iter();
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
                ["paired", _, role, asked] => {
                    let within = |n: f64| (2.0..6.0).contains(&n);
                    let kept = |e: &Entity| match asked.parse::<i64>() {
                        Ok(probe) => holds(e.fields.get("a"), &Json::from(probe)),
                        Err(_) => in_range(e.fields.get("a"), &within),
                    };
                    json!(
                        of_type()
                            .filter(|(_, e)| !e.trashed && e.roles.iter().any(|r| r == role) && kept(e))
                            .map(|(id, _)| id)
                            .collect::<Vec<_>>()
                    )
                }
                ["order", field, direction, limit] => {
                    let mut ordered: Vec<(&String, &Entity)> = of_type().filter(|(_, e)| !e.trashed).copied().collect();
                    ordered.sort_by(|(_, x), (_, y)| {
                        let o = ordered_as(&collator, x.fields.get(*field), y.fields.get(*field));
                        if *direction == "desc" { o.reverse() } else { o }
                    });
                    let take = limit.parse().unwrap_or(usize::MAX);
                    json!(ordered.into_iter().take(take).map(|(id, _)| id).collect::<Vec<_>>())
                }
                ["range", field, low, high] => {
                    let low: f64 = low.parse().unwrap();
                    let within = |n: f64| match high.parse::<f64>() {
                        Ok(high) => low <= n && n < high,
                        Err(_) => low < n,
                    };
                    json!(
                        of_type()
                            .filter(|(_, e)| !e.trashed && in_range(e.fields.get(*field), &within))
                            .map(|(id, _)| id)
                            .collect::<Vec<_>>()
                    )
                }
                ["ranged", field, direction] => {
                    let within = |n: f64| (2.0..6.0).contains(&n);
                    let mut ordered: Vec<(&String, &Entity)> = of_type()
                        .filter(|(_, e)| !e.trashed && in_range(e.fields.get(*field), &within))
                        .copied()
                        .collect();
                    ordered.sort_by(|(_, x), (_, y)| {
                        let o = ordered_as(&collator, x.fields.get(*field), y.fields.get(*field));
                        if *direction == "desc" { o.reverse() } else { o }
                    });
                    json!(ordered.into_iter().take(2).map(|(id, _)| id).collect::<Vec<_>>())
                }
                ["filtered", field, direction] => {
                    let other = if *field == "a" { "b" } else { "a" };
                    let mut ordered: Vec<(&String, &Entity)> = of_type()
                        .filter(|(_, e)| !e.trashed && holds(e.fields.get(other), &json!(3)))
                        .copied()
                        .collect();
                    ordered.sort_by(|(_, x), (_, y)| {
                        let o = ordered_as(&collator, x.fields.get(*field), y.fields.get(*field));
                        if *direction == "desc" { o.reverse() } else { o }
                    });
                    json!(ordered.into_iter().take(2).map(|(id, _)| id).collect::<Vec<_>>())
                }
                ["filtered", "range", field, _] => {
                    let other = if *field == "a" { "b" } else { "a" };
                    let within = |n: f64| (2.0..6.0).contains(&n);
                    let kept =
                        |e: &Entity| holds(e.fields.get(other), &json!(3)) && in_range(e.fields.get(*field), &within);
                    json!(of_type().filter(|(_, e)| !e.trashed && kept(e)).map(|(id, _)| id).collect::<Vec<_>>())
                }
                ["filtered", "page", field, direction, cursor] => {
                    let other = if *field == "a" { "b" } else { "a" };
                    let mut ordered: Vec<(&String, &Entity)> = of_type()
                        .filter(|(_, e)| !e.trashed && holds(e.fields.get(other), &json!(3)))
                        .copied()
                        .collect();
                    ordered.sort_by(|(_, x), (_, y)| {
                        let o = ordered_as(&collator, x.fields.get(*field), y.fields.get(*field));
                        if *direction == "desc" { o.reverse() } else { o }
                    });
                    json!(page_after(ordered.into_iter().map(|(id, _)| id), cursor))
                }
                ["page", field, direction, cursor] => {
                    let mut ordered: Vec<(&String, &Entity)> = of_type().filter(|(_, e)| !e.trashed).copied().collect();
                    ordered.sort_by(|(_, x), (_, y)| {
                        let o = ordered_as(&collator, x.fields.get(*field), y.fields.get(*field));
                        if *direction == "desc" { o.reverse() } else { o }
                    });
                    json!(page_after(ordered.into_iter().map(|(id, _)| id), cursor))
                }
                ["after", cursor] => {
                    json!(page_after(of_type().filter(|(_, e)| !e.trashed).map(|(id, _)| *id), cursor))
                }
                ["holders", "after", cursor] => json!(page_after(
                    of_type().filter(|(_, e)| !e.trashed && holds(e.fields.get("a"), &json!(3))).map(|(id, _)| *id),
                    cursor
                )),
                ["first", n, "where", field, probe] => {
                    let probe = Json::from(probe.parse::<i64>().unwrap());
                    json!(
                        of_type()
                            .filter(|(_, e)| !e.trashed && holds(e.fields.get(*field), &probe))
                            .take(n.parse().unwrap())
                            .map(|(id, _)| id)
                            .collect::<Vec<_>>()
                    )
                }
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

/// The rank index the harness declares, over every entity and kind of edge the workload writes
const RANKED: RankIndex<'static> =
    RankIndex { name: "modelRank", types: &["Task", "Note"], kinds: &ALL_KINDS, direction: "both", weights: true };

const TOLERANCE: f64 = 1e-9;

/// The schema with the spare field a renameField moves values into: unindexed, so the workload's indexes are the same as
/// before. Every database the model compares declares it, the crash child's included
fn with_spare(mut schema: Json) -> Json {
    schema["types"]["Task"]["fields"]["spare"] = json!({ "type": "any", "optional": true });
    schema
}

/// The engine tests' schema with `RANKED` declared, the spare field included, opened over a fresh directory
fn open(dir: &tempfile::TempDir) -> Database {
    let mut schema = schema_json();
    // Change order is what `changed_since` answers from: each type it is asked of is declared to keep it
    for entity_type in ["Task", "Note"] {
        schema["types"][entity_type]["changeOrder"] = json!(true);
    }
    schema = with_spare(schema);
    schema["ranks"] = json!({ RANKED.name: { "types": RANKED.types, "relations": RANKED.kinds,
                                              "direction": RANKED.direction, "weights": RANKED.weights,
                                              "tolerance": TOLERANCE } });
    let schema = Schema::from_json(&schema).unwrap();
    Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap()
}

/// The engine under the model: the prelude written, each step run and its answer compared, and after each the data, every
/// derived key, every derived read and the change set the commit made compared with the model's; after the workload,
/// `check()` finds nothing
struct Engine {
    db: Database,
    _dir: tempfile::TempDir,
    /// The model as it was before the step being tracked, which the step's change is the difference from
    before: Model,
    since: Since,
    /// The entities of `Task` and `Note`, which `changed_since` is asked about
    types: Vec<String>,
    /// The sequence of the last commit the engine reported
    sequence: u64,
    /// Whether a reset has happened, and the position a reader from after it starts at
    replaced: bool,
    replaced_at: Option<u64>,
}

/// What `changed_since` answers from position zero: each entity still there with the sequence of its last change, and
/// those destroyed and not made again
#[derive(Default)]
struct Since {
    last: BTreeMap<String, u64>,
    gone: BTreeSet<String>,
}

/// What a commit did to one entity, as the engine's change set names it: the fields are the ones the commit wrote, the
/// engine's own left out
#[derive(Debug, Clone, Default, PartialEq)]
struct Change {
    created: bool,
    destroyed: bool,
    trashed: bool,
    restored: bool,
    /// The roles the entity holds differ: a grant or a revoke, which the engine names as its `role` field
    roles: bool,
    fields: BTreeSet<String>,
    /// Destroyed and made again within the commit: named with only what was done to the one made. Internal to the replay
    replaced: bool,
}

/// The model's changes between two of its states, each entity the commit changed, with the fields whose values differ
fn changes_between(before: &Model, after: &Model) -> BTreeMap<String, Change> {
    let mut out = BTreeMap::new();
    let ids: BTreeSet<&String> = before.entities.keys().chain(after.entities.keys()).collect();
    for id in ids {
        let change = match (before.entities.get(id), after.entities.get(id)) {
            (None, Some(a)) => {
                Change { created: true, fields: a.fields.keys().cloned().collect(), ..Default::default() }
            }
            (Some(_), None) => Change { destroyed: true, ..Default::default() },
            (Some(b), Some(a)) => {
                let fields = b
                    .fields
                    .keys()
                    .chain(a.fields.keys())
                    .filter(|k| b.fields.get(*k) != a.fields.get(*k))
                    .cloned()
                    .collect();
                let (mut was, mut now) = (b.roles.clone(), a.roles.clone());
                was.sort();
                now.sort();
                Change {
                    trashed: !b.trashed && a.trashed,
                    restored: b.trashed && !a.trashed,
                    roles: was != now,
                    fields,
                    ..Default::default()
                }
            }
            (None, None) => continue,
        };
        if change != Change::default() {
            out.insert(id.clone(), Change { fields: without_engine_fields(change.fields), ..change });
        }
    }
    out
}

fn without_engine_fields(fields: BTreeSet<String>) -> BTreeSet<String> {
    fields.into_iter().filter(|f| !ENGINE_FIELDS.contains(&f.as_str())).collect()
}

/// The changes a commit's change set names, the engine's own fields left out and an entity that changed in no way the
/// model holds left out
fn changes_of(changes: &ChangeSet) -> BTreeMap<String, Change> {
    changes
        .entities
        .iter()
        .map(|(id, e)| {
            let change = Change {
                created: e.created,
                destroyed: e.destroyed,
                trashed: e.trashed,
                restored: e.restored,
                roles: e.fields.iter().any(|f| f == "role"),
                fields: without_engine_fields(e.fields.iter().cloned().collect()),
                replaced: false,
            };
            (id.clone(), change)
        })
        .filter(|(_, change)| *change != Change::default())
        .collect()
}

/// What a step that committed made: each entity it changed and the ends of the edges it changed. The step is replayed on the
/// model one member at a time, so a write a later member reverts is still named, as the engine names it, and a savepoint
/// the model rolls back makes none; the replay must land on the state the reference holds after the step
fn effects_of(step: &Step, before: &Model, after: &Model) -> (BTreeMap<String, Change>, BTreeSet<String>) {
    let mut current = before.clone();
    let mut changes = BTreeMap::new();
    let mut ends = BTreeSet::new();
    effects_in(&mut current, step, &mut changes, &mut ends).expect("a committed step applies");
    for change in changes.values_mut() {
        change.replaced = false;
    }
    // A write that changes nothing the engine names (an upsert of no fields) leaves no entry
    changes.retain(|_, change| *change != Change::default());
    // An entity the commit found and left destroyed is named as destroyed alone, though a unit made it again on the way
    for (id, change) in changes.iter_mut() {
        if before.entities.contains_key(id) && change.created && change.destroyed {
            change.created = false;
        }
    }
    assert!(
        current.entities == after.entities && current.edges == after.edges && current.keyed == after.keyed,
        "replaying a committed step lands on the reference's state: {step:?}"
    );
    (changes, ends)
}

/// Applies one step to `current`, adding what it made. A unit's members in turn; a unit inside another is a savepoint, and
/// one that fails or throws is rolled back with its effects. Err when a leaf step is refused
fn effects_in(
    current: &mut Model,
    step: &Step,
    changes: &mut BTreeMap<String, Change>,
    ends: &mut BTreeSet<String>,
) -> Result<(), Refusal> {
    match step {
        Step::Unit { steps, throws, .. } => {
            let saved = current.clone();
            let (mut inner_changes, mut inner_ends) = (BTreeMap::new(), BTreeSet::new());
            let outcome =
                steps.iter().try_for_each(|member| effects_in(current, member, &mut inner_changes, &mut inner_ends));
            if outcome.is_err() || *throws {
                *current = saved;
                return Ok(());
            }
            merge(changes, inner_changes);
            ends.extend(inner_ends);
            Ok(())
        }
        leaf => {
            let previous = current.clone();
            current.apply(leaf)?;
            merge(changes, changes_between(&previous, current));
            merge(changes, writes_of(leaf, &previous));
            ends.extend(touched_ends(&previous, current));
            ends.extend(written_ends(leaf));
            Ok(())
        }
    }
}

/// One relation as the engine's change set names it: an edge made, unlinked, rekeyed or moved, by its kind and ends. A relation
/// the commit made and unlinked again is one entry with both flags, as the engine reports it
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Rel {
    kind: String,
    source: String,
    target: String,
    linked: bool,
    unlinked: bool,
    changed: bool,
    /// The ends a moved relation had before the commit
    moved_from: Option<(String, String)>,
    /// The key of a keyed edge, which tells two of the same ends apart. Internal to the replay: the engine's report has
    /// no key, so it is cleared before the comparison
    n: Option<u8>,
    /// The key a rekey gave the edge, which is what the entry's key becomes. Internal to the replay
    to_n: Option<u8>,
}

impl Rel {
    fn new(kind: &str, source: &str, target: &str) -> Rel {
        Rel {
            kind: kind.to_owned(),
            source: source.to_owned(),
            target: target.to_owned(),
            linked: false,
            unlinked: false,
            changed: false,
            moved_from: None,
            n: None,
            to_n: None,
        }
    }
}

/// The relations a committed step made, in the order of the engine's report. Replayed as `effects_in` replays the entities
fn relations_of(step: &Step, before: &Model) -> Vec<Rel> {
    let mut current = before.clone();
    let mut rels = Vec::new();
    rel_effects_in(&mut current, step, &mut rels).expect("a committed step applies");
    for rel in &mut rels {
        rel.n = None;
        rel.to_n = None;
    }
    rels.sort();
    rels
}

fn rel_effects_in(current: &mut Model, step: &Step, rels: &mut Vec<Rel>) -> Result<(), Refusal> {
    match step {
        Step::Unit { steps, throws, .. } => {
            // A nested unit's events go straight into the commit's list, so each finds the entry it follows by the key the
            // edge had then; a savepoint that fails or throws is rolled back with its events
            let saved = current.clone();
            let saved_rels = rels.clone();
            let outcome = steps.iter().try_for_each(|member| rel_effects_in(current, member, rels));
            if outcome.is_err() || *throws {
                *current = saved;
                *rels = saved_rels;
            }
            Ok(())
        }
        leaf => {
            let previous = current.clone();
            current.apply(leaf)?;
            let events = match leaf {
                Step::MoveRelation { id, kind, target, to } => {
                    let mut rel = Rel::new(kind, id, to);
                    rel.changed = true;
                    // Moved onto the target it has, the relation is changed and has moved from nowhere
                    rel.moved_from = (to != target).then(|| (id.to_string(), target.to_string()));
                    vec![rel]
                }
                Step::Rekey { id, target, from, to } => {
                    let mut rel = Rel::new(KEYED, id, target);
                    rel.changed = true;
                    rel.n = Some(*from);
                    rel.to_n = Some(*to);
                    vec![rel]
                }
                _ => edge_diff(&previous, current),
            };
            for rel in events {
                push_rel(rels, rel);
            }
            Ok(())
        }
    }
}

/// The links and unlinks between two states: an edge added is a relation made, one removed a relation unlinked
fn edge_diff(before: &Model, after: &Model) -> Vec<Rel> {
    let mut out = Vec::new();
    for (source, kind, target) in after.edges.difference(&before.edges) {
        let mut rel = Rel::new(kind, source, target);
        rel.linked = true;
        out.push(rel);
    }
    for (source, kind, target) in before.edges.difference(&after.edges) {
        let mut rel = Rel::new(kind, source, target);
        rel.unlinked = true;
        out.push(rel);
    }
    for (source, target, n) in after.keyed.difference(&before.keyed) {
        let mut rel = Rel::new(KEYED, source, target);
        rel.linked = true;
        rel.n = Some(*n);
        out.push(rel);
    }
    for (source, target, n) in before.keyed.difference(&after.keyed) {
        let mut rel = Rel::new(KEYED, source, target);
        rel.unlinked = true;
        rel.n = Some(*n);
        out.push(rel);
    }
    out
}

/// Records a relation event: an unlink of a relation the commit already has an entry for, linked or rekeyed or moved, is
/// folded into that entry; anything else is an entry of its own
fn push_rel(rels: &mut Vec<Rel>, rel: Rel) {
    // A move is the same relation as the entry holding its pre-move ends, and that entry keeps the ends it began with
    if let Some((from_source, from_target)) = rel.moved_from.clone()
        && let Some(open) = rels.iter_mut().find(|r| {
            !r.unlinked && r.kind == rel.kind && r.source == from_source && r.target == from_target && r.n == rel.n
        })
    {
        open.source = rel.source;
        open.target = rel.target;
        open.changed = true;
        open.moved_from.get_or_insert((from_source, from_target));
        return;
    }
    // A link makes a relation of its own; an unlink or a rekey is the same relation as an entry it follows, for the same ends
    if !rel.linked
        && rel.moved_from.is_none()
        && let Some(open) = rels.iter_mut().find(|r| {
            !r.unlinked && r.kind == rel.kind && r.source == rel.source && r.target == rel.target && r.n == rel.n
        })
    {
        open.unlinked |= rel.unlinked;
        open.changed |= rel.changed;
        if rel.to_n.is_some() {
            open.n = rel.to_n;
        }
        return;
    }
    // An entry carries the key its edge has after the commit, so an event later in the commit finds it by that key
    let mut rel = rel;
    if let Some(to) = rel.to_n.take() {
        rel.n = Some(to);
    }
    rels.push(rel);
}

/// The fields and roles a write names, whether or not its value changed: the engine reports each one it wrote. A grant of a
/// role the entity holds, or a revoke of one it lacks, writes nothing
fn writes_of(step: &Step, previous: &Model) -> BTreeMap<String, Change> {
    let field = |id: &str, names: Vec<&str>| {
        let fields = names.into_iter().map(str::to_owned).collect();
        [(id.to_owned(), Change { fields, ..Default::default() })].into_iter().collect()
    };
    let held = |id: &str, key: &str| previous.entities.get(id).and_then(|e| e.fields.get(key));
    match step {
        Step::Set { id, key, .. } | Step::Append { id, key, .. } | Step::Merge { id, key, .. } => field(id, vec![key]),
        // Removals that take nothing away write nothing: a missing field, or an index past the list's end
        Step::Delete { id, key } => {
            if held(id, key).is_some() {
                field(id, vec![key])
            } else {
                BTreeMap::new()
            }
        }
        Step::RemoveAt { id, key, index } => match held(id, key) {
            Some(Json::Array(items)) if (*index as usize) < items.len() => field(id, vec![key]),
            _ => BTreeMap::new(),
        },
        Step::RemoveValue { id, key, value } => match held(id, key) {
            Some(Json::Array(items)) if items.contains(value) => field(id, vec![key]),
            _ => BTreeMap::new(),
        },
        Step::Create { id, fields } | Step::Upsert { id, fields } => field(id, fields.keys().copied().collect()),
        Step::Grant { id, role } | Step::GrantExclusive { id, role } | Step::Revoke { id, role } => {
            let held = previous.entities.get(*id).is_some_and(|e| e.roles.iter().any(|r| r == role));
            let writes = if matches!(step, Step::Revoke { .. }) { held } else { !held };
            if writes {
                [(id.to_string(), Change { roles: true, ..Default::default() })].into_iter().collect()
            } else {
                BTreeMap::new()
            }
        }
        _ => BTreeMap::new(),
    }
}

/// The ends a step writes whether or not its edge changes: a rekey stamps both, to its own key or another, and a move
fn written_ends(step: &Step) -> BTreeSet<String> {
    match step {
        Step::Rekey { id, target, .. } => [id.to_string(), target.to_string()].into_iter().collect(),
        // A move wrote the ends it left and the one it joined, though to the same target it changes nothing
        Step::MoveRelation { id, target, to, .. } => {
            [id.to_string(), target.to_string(), to.to_string()].into_iter().collect()
        }
        _ => BTreeSet::new(),
    }
}

/// Adds the changes of a later step to those of the steps before it. An entity destroyed and made again within the commit is
/// named as made alone, with only what was done to the one made, as `ChangeSet::entities` documents
fn merge(into: &mut BTreeMap<String, Change>, from: BTreeMap<String, Change>) {
    for (id, later) in from {
        match into.get_mut(&id) {
            Some(earlier) if later.replaced || (later.created && earlier.destroyed) => {
                *earlier = Change { replaced: true, ..later };
            }
            Some(earlier) => {
                earlier.created |= later.created;
                earlier.destroyed |= later.destroyed;
                earlier.trashed |= later.trashed;
                earlier.restored |= later.restored;
                earlier.roles |= later.roles;
                earlier.fields.extend(later.fields);
            }
            None => {
                into.insert(id, later);
            }
        }
    }
}

impl Since {
    fn record(&mut self, sequence: u64, changes: &BTreeMap<String, Change>) {
        for (id, change) in changes {
            if change.destroyed {
                self.last.remove(id);
                self.gone.insert(id.clone());
            } else {
                self.gone.remove(id);
                self.last.insert(id.clone(), sequence);
            }
        }
    }
}

/// The entities at either end of an edge a step added, removed or moved, or of a keyed edge whose key changed. A relation
/// change stamps both its ends in `changed_since`, though the change set names them as changed only in its relations
fn touched_ends(before: &Model, after: &Model) -> BTreeSet<String> {
    let mut ends = BTreeSet::new();
    for (source, _, target) in before.edges.symmetric_difference(&after.edges) {
        ends.insert(source.clone());
        ends.insert(target.clone());
    }
    for (source, target, _) in before.keyed.symmetric_difference(&after.keyed) {
        ends.insert(source.clone());
        ends.insert(target.clone());
    }
    ends
}

/// Records the model's state after a step, comparing the change set the engine made with the change the model's states
/// name, and what `changed_since` answers from the start with what the changes so far say. `quiet` is the database's
/// sequence as the step began, with the rank index's builder idle: a rename reports no sequence, and its commit is
/// the one after that. The database's own sequence by now may be past it, by a commit of the builder's the rename
/// gave work to
fn track(engine: &mut Engine, step: &Step, got: &Result<Option<ChangeSet>, crate::Error>, after: Model, quiet: u64) {
    if let (Step::RenameField { from, .. }, Ok(_)) = (step, got) {
        // A field rename commits, consuming a sequence even when no entity held the field; it stamps the entities that did
        let sequence = quiet + 1;
        assert!(sequence > engine.sequence, "{step:?}: a commit's sequence only increases");
        engine.sequence = sequence;
        let holders: Vec<String> = engine
            .before
            .entities
            .iter()
            .filter(|(id, e)| crate::engine::ids::type_of(id) == "Task" && e.fields.contains_key(*from))
            .map(|(id, _)| id.clone())
            .collect();
        for id in holders {
            if after.entities.contains_key(&id) {
                engine.since.gone.remove(&id);
                engine.since.last.insert(id, sequence);
            }
        }
        if engine.replaced && engine.replaced_at.is_none() {
            engine.replaced_at = Some(sequence - 1);
        }
        engine.before = after;
        assert_since(engine, step);
        return;
    }
    if let (Step::RenameRelation { from, .. }, Ok(_)) = (step, got) {
        // A rename commits no change set and reports no sequence: its commit is the one after the database's as the
        // step began. It stamps the ends of each edge it moved, and the ends are alive
        let sequence = quiet + 1;
        assert!(sequence > engine.sequence, "{step:?}: a commit's sequence only increases");
        engine.sequence = sequence;
        let moved: Vec<(String, String)> = engine
            .before
            .edges
            .iter()
            .filter(|(_, kind, _)| kind.as_str() == *from)
            .map(|(source, _, target)| (source.clone(), target.clone()))
            .collect();
        for id in moved.into_iter().flat_map(|(source, target)| [source, target]) {
            if after.entities.contains_key(&id) {
                engine.since.gone.remove(&id);
                engine.since.last.insert(id, sequence);
            }
        }
        if engine.replaced && engine.replaced_at.is_none() {
            engine.replaced_at = Some(sequence - 1);
        }
        engine.before = after;
        assert_since(engine, step);
        return;
    }
    if let (Step::Reset, Ok(_)) = (step, got) {
        // A reset commits no change set, and a reader from before it is told the answer is incomplete: the entities it
        // removed have no tombstone. The tracking starts again from the database as it is
        engine.before = after;
        engine.since = Since::default();
        engine.replaced = true;
        engine.replaced_at = None;
        assert_since(engine, step);
        return;
    }
    match got {
        Ok(Some(commit)) => {
            let (changes, ends) = effects_of(step, &engine.before, &after);
            let mut reported: Vec<Rel> = commit
                .relations
                .values()
                .map(|r| Rel {
                    kind: r.kind.clone(),
                    source: r.source.clone(),
                    target: r.target.clone(),
                    linked: r.linked,
                    unlinked: r.unlinked,
                    changed: r.changed,
                    moved_from: r.moved_from.as_ref().map(|e| (e.source.clone(), e.target.clone())),
                    n: None,
                    to_n: None,
                })
                .collect();
            reported.sort();
            assert_eq!(reported, relations_of(step, &engine.before), "{step:?}: the relations the commit reported");
            assert!(commit.sequence > engine.sequence, "{step:?}: a commit's sequence only increases");
            engine.sequence = commit.sequence;
            // A reader from the commit before the first after a reset reads what changed since the reset
            if engine.replaced && engine.replaced_at.is_none() {
                engine.replaced_at = Some(commit.sequence - 1);
            }
            assert_eq!(changes_of(commit), changes, "{step:?}: the change set the commit reported");
            engine.since.record(commit.sequence, &changes);
            // An end the step left destroyed is not stamped: its destruction is the change
            for id in ends.iter().filter(|id| after.entities.contains_key(*id)) {
                engine.since.last.insert(id.clone(), commit.sequence);
            }
        }
        _ => {
            assert!(
                changes_between(&engine.before, &after).is_empty(),
                "{step:?}: the model changed where the engine made no commit"
            );
            assert!(
                touched_ends(&engine.before, &after).is_empty(),
                "{step:?}: the model moved an edge where the engine made no commit"
            );
        }
    }
    engine.before = after;
    assert_since(engine, step);
}

fn assert_since(engine: &Engine, step: &Step) {
    let from_start = engine
        .db
        .changed_since(&engine.types, &crate::engine::changed::Position::default(), 1_000_000)
        .expect("changed_since answers");
    if engine.replaced {
        assert!(!from_start.complete, "{step:?}: a reader from before a reset is told the answer is incomplete");
    } else {
        assert_answer(engine, step, &from_start);
    }
    if let Some(sequence) = engine.replaced_at {
        let position = crate::engine::changed::Position { sequence, after: None };
        let since = engine.db.changed_since(&engine.types, &position, 1_000_000).expect("changed_since answers");
        assert_answer(engine, step, &since);
    }
}

/// What `changed_since` answers, which must be the entities still there with the sequence of their last change, and the
/// entities destroyed and not made again, and everything
fn assert_answer(engine: &Engine, step: &Step, got: &crate::engine::changed::Changed) {
    assert_eq!(got.ids.len(), got.sequences.len(), "each changed entity has the sequence of its last change");
    let ids: BTreeMap<String, u64> = got.ids.iter().cloned().zip(got.sequences.iter().copied()).collect();
    assert_eq!(got.ids.len(), ids.len(), "each changed entity is named once");
    assert_eq!(
        ids, engine.since.last,
        "{step:?}: changed since the start, each entity still there and the commit of its last change"
    );
    let destroyed: BTreeSet<String> = got.destroyed.iter().cloned().collect();
    assert_eq!(got.destroyed.len(), destroyed.len(), "each destroyed entity is named once");
    assert_eq!(destroyed, engine.since.gone, "changed since the start: the entities destroyed and not made again");
    assert!(got.done && got.complete, "changed since the start answers everything: {got:?}");
}

struct EngineMachine;

impl proptest_state_machine::StateMachineTest for EngineMachine {
    type SystemUnderTest = Engine;
    type Reference = ModelMachine;

    fn init_test(reference: &Reference) -> Engine {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let mut engine = Engine {
            db,
            _dir: dir,
            before: Model::default(),
            since: Since::default(),
            types: vec!["Task".to_owned(), "Note".to_owned()],
            sequence: 0,
            replaced: false,
            replaced_at: None,
        };
        for step in &reference.prelude {
            let quiet = engine.db.sequence().expect("the sequence reads");
            let got = run(&engine.db, step);
            assert!(got.is_ok(), "the prelude's {step:?}: {got:?}");
            let mut after = engine.before.clone();
            after.step(step).expect("the prelude creates each id once");
            track(&mut engine, step, &got, after, quiet);
        }
        engine
    }

    fn apply(mut engine: Engine, reference: &Reference, transition: Aimed) -> Engine {
        // Read with the builder idle, so that the step's commit is the next: the builder commits what a step before
        // gave it to do, and is waited for here whether or not the invariants after that step were
        engine.db.wait_for_index_builds().expect("the builder's drain ends");
        let quiet = engine.db.sequence().expect("the sequence reads");
        let got = run(&engine.db, &transition.step);
        assert_eq!(
            got.is_ok(),
            reference.outcome.is_ok(),
            "{:?}: the engine said {got:?}, the model {:?}",
            transition.step,
            reference.outcome
        );
        track(&mut engine, &transition.step, &got, reference.model.clone(), quiet);
        engine
    }

    fn check_invariants(engine: &Engine, reference: &Reference) {
        assert_eq!(state_of(&engine.db), model_state(&reference.model));
        assert_eq!(sorted_far_ends(derived_of(&engine.db)), derived_from(&reference.model));
        assert_eq!(plans_of(&engine.db), plans_from(&reference.model));
        // A registered field outlives its values, so what the model holds is all it must include
        engine
            .db
            .snapshot(|s| {
                for (id, entity) in &reference.model.entities {
                    for field in entity.fields.keys() {
                        let key = keys::field_name(crate::engine::ids::type_of(id), field);
                        assert!(s.get(&key).unwrap().is_some(), "{id}'s {field} isn't registered");
                    }
                }
                Ok(())
            })
            .unwrap();
        // The drain waited for, so no commit of its own takes a refusal the next step armed
        let live: Vec<String> =
            reference.model.entities.iter().filter(|(_, e)| !e.trashed).map(|(id, _)| id.clone()).collect();
        assert_eq!(RANKED.assert_within_bound(&engine.db), live, "the rank index's nodes");
        // Any state of the index is within the bound it reports, so that bound is what says the drain ran. Its report is
        // rounded up, a step at most about the tolerance
        let (_, error) = RANKED.indexed(&engine.db);
        assert!(error <= 2.0 * TOLERANCE, "the drain left a bound of {error}");
    }

    fn teardown(engine: Engine, _: Reference) {
        crate::failpoints::disarm_at(crate::failpoints::points::COMMIT, engine.db.path());
        let report = engine.db.check().unwrap();
        assert!(report.is_clean(), "check() after the workload: {report:?}");
    }
}

#[test]
fn a_unit_that_destroys_makes_and_destroys_an_entity_it_found_names_it_as_destroyed_alone() {
    // The engine names an entity the commit found and left destroyed as destroyed alone, though the commit made it again
    // on the way; the model's union of the members' changes must say so too
    let prelude = vec![Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) }];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude creates the id once");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::Unit {
        steps: vec![
            Step::Destroy { id: "Task-1" },
            Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("y"))]) },
            Step::Destroy { id: "Task-1" },
        ],
        throws: false,
        durable: false,
    };
    reference.outcome = model.step(&step);
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
fn a_unit_that_grants_then_destroys_an_entity_names_the_role_on_the_destroyed_entity() {
    // A grant made before the destroy in the commit is the destroyed entity's `role` field, as the engine names it
    let prelude = vec![Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) }];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude creates the id once");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::Unit {
        steps: vec![Step::Grant { id: "Task-1", role: "r1" }, Step::Destroy { id: "Task-1" }],
        throws: false,
        durable: false,
    };
    reference.outcome = model.step(&step);
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
fn a_destroy_and_remake_in_a_nested_unit_drops_what_the_outer_unit_wrote_before_it() {
    // The engine names a destroyed-and-made entity with only what was done to the one made: the fields an enclosing unit
    // wrote before the nested unit's destroy are not among them
    let prelude = vec![Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) }];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude creates the id once");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::Unit {
        steps: vec![
            Step::Set { id: "Task-1", key: "c", value: json!("y") },
            Step::Unit {
                steps: vec![Step::Destroy { id: "Task-1" }, Step::Upsert { id: "Task-1", fields: BTreeMap::new() }],
                throws: false,
                durable: false,
            },
        ],
        throws: false,
        durable: false,
    };
    reference.outcome = model.step(&step);
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
fn a_rename_is_tracked_at_its_own_commit_though_the_builder_has_committed_since() {
    // A rename reports no sequence, and gives the rank index's builder work it commits by itself: by the time the
    // step is tracked the database's sequence may be the builder's, a commit past the one that stamped the ends
    let prelude = vec![
        Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-3", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Link { id: "Task-1", kind: "refs", target: "Task-2" },
        Step::Link { id: "Task-2", kind: "refs", target: "Task-3" },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let mut engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    engine.db.wait_for_index_builds().unwrap();
    let quiet = engine.db.sequence().unwrap();
    let step = Step::RenameRelation { from: "refs", to: "tags" };
    let got = run(&engine.db, &step);
    engine.db.wait_for_index_builds().unwrap();
    assert_eq!(engine.db.sequence().unwrap(), quiet + 2, "the rename's commit, and the builder's after it");
    model.step(&step).expect("the model renames");
    track(&mut engine, &step, &got, model, quiet);
}

#[test]
fn a_move_onto_the_target_a_relation_has_is_reported_as_a_change_and_as_no_move() {
    // The workload moves a relation onto any entity, the target it has among them: the engine reports the relation
    // changed and not moved. Alone, and after a link in the same commit, it names no ends it moved from; after a move
    // in the same commit, the ends that move took it from
    let prelude = vec![
        Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-3", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Link { id: "Task-2", kind: "refs", target: "Task-1" },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let mut engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let onto_its_own = Step::MoveRelation { id: "Task-2", kind: "refs", target: "Task-1", to: "Task-1" };
    let moved_then_onto_its_own = Step::Unit {
        steps: vec![
            Step::MoveRelation { id: "Task-2", kind: "refs", target: "Task-1", to: "Task-3" },
            Step::MoveRelation { id: "Task-2", kind: "refs", target: "Task-3", to: "Task-3" },
        ],
        throws: false,
        durable: false,
    };
    let linked_then_onto_its_own = Step::Unit {
        steps: vec![
            Step::Link { id: "Task-3", kind: "refs", target: "Task-1" },
            Step::MoveRelation { id: "Task-3", kind: "refs", target: "Task-1", to: "Task-1" },
        ],
        throws: false,
        durable: false,
    };
    for step in [onto_its_own, moved_then_onto_its_own, linked_then_onto_its_own] {
        reference.outcome = model.step(&step);
        assert_eq!(reference.outcome, Ok(()), "{step:?}");
        reference.model = model.clone();
        engine = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
            engine,
            &reference,
            Aimed { step, aimed: true },
        );
    }
}

#[test]
fn two_moves_of_one_relation_in_one_commit_are_one_entry_with_its_first_ends() {
    // The engine names a relation moved twice in one commit once, changed, at its last ends and with the ends it began with
    let prelude = vec![
        Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-3", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-4", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Link { id: "Task-1", kind: "refs", target: "Task-2" },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::Unit {
        steps: vec![
            Step::MoveRelation { id: "Task-1", kind: "refs", target: "Task-2", to: "Task-3" },
            Step::MoveRelation { id: "Task-1", kind: "refs", target: "Task-3", to: "Task-4" },
        ],
        throws: false,
        durable: false,
    };
    reference.outcome = model.step(&step);
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
fn a_rename_onto_an_edge_of_the_new_kind_with_the_same_ends_is_refused_by_both() {
    // Renaming refs to tags, where tags already joins the same two entities, would leave two edges with one identity: the
    // engine refuses it, and the model must refuse it the same way, moving nothing
    let prelude = vec![
        Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Link { id: "Task-1", kind: "tags", target: "Task-2" },
        Step::Link { id: "Task-1", kind: "refs", target: "Task-2" },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::RenameRelation { from: "refs", to: "tags" };
    reference.outcome = model.step(&step);
    assert!(reference.outcome.is_err(), "the model refuses the rename");
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
fn a_refused_rename_leaves_an_armed_refusal_for_the_next_commit_as_the_engine_does() {
    // The engine refuses a rename onto an edge of the new kind before its commit, so the refusal armed for the next commit
    // stays armed: the create after the failed rename is the one refused, by both
    let prelude = vec![
        Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Link { id: "Task-1", kind: "tags", target: "Task-2" },
        Step::Link { id: "Task-1", kind: "refs", target: "Task-2" },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let mut engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let steps = [
        Step::RefuseNext,
        Step::RenameRelation { from: "refs", to: "tags" },
        Step::Create { id: "Task-3", fields: BTreeMap::from([("a", json!("y"))]) },
    ];
    for step in steps {
        reference.outcome = model.step(&step);
        reference.model = model.clone();
        engine = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
            engine,
            &reference,
            Aimed { step, aimed: false },
        );
    }
    assert!(reference.outcome.is_err(), "the create after the failed rename is refused, as the armed refusal says");
}

#[test]
fn a_field_rename_onto_a_name_the_task_holds_is_refused_by_both() {
    // A Task holding both c and spare would have one value replaced: the engine refuses the rename and the model must too,
    // moving nothing
    let prelude = vec![Step::Create { id: "Task-1", fields: BTreeMap::from([("c", json!(1)), ("spare", json!(2))]) }];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude creates the id once");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::RenameField { from: "c", to: "spare" };
    reference.outcome = model.step(&step);
    assert!(reference.outcome.is_err(), "the model refuses the rename");
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
fn a_rekey_in_a_nested_unit_is_the_same_relation_as_an_earlier_rekey_of_it_in_the_commit() {
    // An outer unit rekeys an edge to the key it has, and a nested unit rekeys it again: one relation, one entry
    let prelude = vec![
        Step::Create { id: "Note-1", fields: BTreeMap::from([("a", json!("t"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::LinkKeyed { id: "Note-1", target: "Task-2", n: 0 },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::Unit {
        steps: vec![
            Step::Rekey { id: "Note-1", target: "Task-2", from: 0, to: 0 },
            Step::Unit {
                steps: vec![Step::Rekey { id: "Note-1", target: "Task-2", from: 0, to: 1 }],
                throws: false,
                durable: false,
            },
        ],
        throws: false,
        durable: false,
    };
    reference.outcome = model.step(&step);
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

proptest_state_machine::prop_state_machine! {
    #![proptest_config(ProptestConfig { cases: 96, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn the_engine_holds_what_the_model_holds_after_every_step(sequential 10..60 => EngineMachine);
}

/// Every kind of step, as `kind_of` names it
const STEP_KINDS: [&str; 27] = [
    "Create",
    "Set",
    "Append",
    "Delete",
    "Link",
    "Unlink",
    "LinkKeyed",
    "Rekey",
    "Destroy",
    "Trash",
    "Restore",
    "Grant",
    "Revoke",
    "Upsert",
    "Merge",
    "RemoveAt",
    "RemoveValue",
    "GrantExclusive",
    "RemoveRelation",
    "MoveRelation",
    "TakeLabel",
    "Unit",
    "RefuseNext",
    "Rebuild",
    "Reset",
    "RenameRelation",
    "RenameField",
];

fn kind_of(step: &Step) -> &'static str {
    match step {
        Step::Create { .. } => "Create",
        Step::Set { .. } => "Set",
        Step::Append { .. } => "Append",
        Step::Delete { .. } => "Delete",
        Step::Link { .. } => "Link",
        Step::Unlink { .. } => "Unlink",
        Step::LinkKeyed { .. } => "LinkKeyed",
        Step::Rekey { .. } => "Rekey",
        Step::Destroy { .. } => "Destroy",
        Step::Trash { .. } => "Trash",
        Step::Restore { .. } => "Restore",
        Step::Grant { .. } => "Grant",
        Step::Revoke { .. } => "Revoke",
        Step::Upsert { .. } => "Upsert",
        Step::Merge { .. } => "Merge",
        Step::RemoveAt { .. } => "RemoveAt",
        Step::RemoveValue { .. } => "RemoveValue",
        Step::GrantExclusive { .. } => "GrantExclusive",
        Step::RemoveRelation { .. } => "RemoveRelation",
        Step::MoveRelation { .. } => "MoveRelation",
        Step::TakeLabel { .. } => "TakeLabel",
        Step::Unit { .. } => "Unit",
        Step::RefuseNext => "RefuseNext",
        Step::Rebuild { .. } => "Rebuild",
        Step::Reset => "Reset",
        Step::RenameRelation { .. } => "RenameRelation",
        Step::RenameField { .. } => "RenameField",
    }
}

/// Generated workloads complete every kind of step many times, and still make each refusal the model has: a kind no
/// workload completes is one the harness never tests, which choosing ids blind made of moving and removing a relation
#[test]
fn generated_workloads_complete_every_kind_of_step_and_make_every_refusal() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    use proptest_state_machine::ReferenceStateMachine;
    // Each reason `Model::apply` and `Model::step` refuse with
    const REFUSALS: [&str; 9] = [
        "id in use",
        "no such entity",
        "not a list",
        "self link",
        "no such relation",
        "duplicate edge",
        "link target missing at commit",
        "the unit threw",
        "the store refused the commit",
    ];
    let mut runner = TestRunner::deterministic();
    let mut completed: BTreeMap<&str, usize> = BTreeMap::new();
    let mut refused: BTreeMap<&str, usize> = BTreeMap::new();
    for _ in 0..200 {
        let (reference, transitions, _) =
            ModelMachine::sequential_strategy(10..60).new_tree(&mut runner).unwrap().current();
        let mut model = reference.model;
        for transition in transitions {
            match model.step(&transition.step) {
                Ok(()) => *completed.entry(kind_of(&transition.step)).or_default() += 1,
                Err(reason) => *refused.entry(reason).or_default() += 1,
            }
        }
    }
    for kind in STEP_KINDS {
        let n = completed.get(kind).copied().unwrap_or(0);
        assert!(n >= 20, "{kind} completed {n} times in 200 workloads: {completed:?}");
    }
    for reason in REFUSALS {
        assert!(refused.contains_key(reason), "no step was refused with {reason:?}: {refused:?}");
    }
}

/// The crash harness (`docs/architecture/testing.md`): a child process runs a workload over a database, acknowledging each step as it
/// commits, and is killed with `SIGKILL` at a random point; the reopened files must hold the model's state after the
/// acknowledged steps, or after one more, the step in flight having committed before its acknowledgement. The child
/// regenerates the workload from its seed, so nothing but the seed crosses.
mod crash {
    use std::io::{BufRead, BufReader, Write as _};
    use std::process::{Command, Stdio};

    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    use proptest_state_machine::ReferenceStateMachine;

    use super::*;

    /// The seed's workload, with no refused commit, which only a backend in this process can inject
    fn seeded(seed: u64) -> Vec<Step> {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        let mut runner = TestRunner::new_with_rng(Config::default(), TestRng::from_seed(RngAlgorithm::ChaCha, &bytes));
        let (reference, transitions, _) =
            ModelMachine::sequential_strategy(10..60).new_tree(&mut runner).unwrap().current();
        reference
            .prelude
            .into_iter()
            .chain(transitions.into_iter().map(|t| t.step))
            .filter(|s| !matches!(s, Step::RefuseNext))
            .collect()
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
    #[ignore = "run only by the next test, as its child process"]
    fn child_runs_a_seeded_workload_acknowledging_each_step() {
        let (Ok(path), Ok(seed)) = (std::env::var("ELMDB_CRASH_DB"), std::env::var("ELMDB_CRASH_SEED")) else { return };
        // Syncs on, as the app runs: a commit survives the process's death before any sync
        let db = Database::open(
            &path,
            crate::OpenOptions {
                schema: Some(
                    crate::schema::Schema::from_json(&super::with_spare(super::super::schema_json())).unwrap(),
                ),
                ..Default::default()
            },
        )
        .unwrap();
        let mut out = std::io::stdout().lock();
        for (i, step) in seeded(seed.parse().unwrap()).iter().enumerate() {
            let _ = run(&db, step);
            writeln!(out, "ack {i}").unwrap();
            out.flush().unwrap();
        }
        writeln!(out, "done").unwrap();
        out.flush().unwrap();
        // Waits to be killed, and ends with the test that started it if that ends first, however it ends: its handle
        // to this stdin closes with it
        let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new());
    }

    #[test]
    fn a_killed_writer_leaves_the_state_after_its_acknowledged_steps_or_one_more() {
        for seed in 0..48u64 {
            let steps = seeded(seed);
            let dir = tempfile::tempdir().unwrap();
            // The child's own temporary directories, a rebuild's scratch among them, inside this test's: killed in the
            // middle of one, it can't remove it, and this directory's removal takes it
            let scratch = dir.path().join("scratch");
            std::fs::create_dir(&scratch).unwrap();
            let path = dir.path().join("db");
            std::fs::create_dir(&path).unwrap();
            // Somewhere past the prelude, and sometimes the whole workload
            let kill_at = 6 + (seed as usize * 7) % steps.len().max(7);
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "engine::tests::model::crash::child_runs_a_seeded_workload_acknowledging_each_step",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("ELMDB_CRASH_DB", &path)
                .env("TMPDIR", &scratch)
                .env("TMP", &scratch)
                .env("TEMP", &scratch)
                .env("ELMDB_CRASH_SEED", seed.to_string())
                .stdin(Stdio::piped())
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
            // A child that never ran its workload leaves no files, which the model's empty state would match
            assert!(acked > 0, "seed {seed}: the child acknowledged no step");
            let db = Database::open(&path, crate::OpenOptions { syncs: false, ..Default::default() }).unwrap();
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
