//! The workload: the steps each state of the model makes likely, and the test that the steps generated complete every
//! kind of step and make every refusal

use super::*;

pub(super) fn value() -> impl Strategy<Value = Json> {
    prop_oneof![
        // Words of a few letters, accented or not, small, capitalised or in capitals, an accent composed or after its
        // letter: the collation orders them apart from their bytes and holds a composed word equal to the same word
        // decomposed, and the text index holds each as the one word it folds to (`folded`)
        4 => ("[a-cáäåç]{0,8}", 0u8..3, any::<bool>())
            .prop_map(|(word, case, apart)| Json::from(spelled(&cased(&word, case), apart))),
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

/// A word small, with its first letter a capital, or in capitals: never a capital after a small letter, where the text
/// index's analyzer would split it in two
pub(super) fn cased(word: &str, case: u8) -> String {
    match case {
        0 => word.to_owned(),
        1 => {
            let mut chars = word.chars();
            chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
        }
        _ => word.to_uppercase(),
    }
}

/// The accents the workload writes, as the marks that follow their letter decomposed, and the letters they compose with
pub(super) const ACCENTS: [(char, char, char, char); 4] =
    [('á', 'Á', 'a', '\u{301}'), ('ä', 'Ä', 'a', '\u{308}'), ('å', 'Å', 'a', '\u{30a}'), ('ç', 'Ç', 'c', '\u{327}')];

/// A word with each accented letter composed, as it was generated, or `apart`: its letter and then its mark
pub(super) fn spelled(word: &str, apart: bool) -> String {
    let mut out = String::new();
    for c in word.chars() {
        match ACCENTS.iter().find(|(small, capital, ..)| apart && (c == *small || c == *capital)) {
            Some((small, _, letter, mark)) => {
                out.push(if c == *small { *letter } else { letter.to_ascii_uppercase() });
                out.push(*mark);
            }
            None => out.push(c),
        }
    }
    out
}

pub(super) fn id() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&IDS[..])
}
pub(super) fn key() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&KEYS[..])
}
pub(super) fn kind() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&KINDS[..])
}
pub(super) fn role() -> impl Strategy<Value = &'static str> {
    prop::sample::select(&ROLES[..])
}
pub(super) fn fields() -> impl Strategy<Value = BTreeMap<&'static str, Json>> {
    prop::collection::btree_map(key(), value(), 0..3)
}

/// One write that isn't a unit
pub(super) fn write() -> BoxedStrategy<Step> {
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
pub(super) struct Aimed {
    pub(super) step: Step,
    pub(super) aimed: bool,
}

/// The model as the state machine carries it: the model, the prelude that made it, and what the last step answered,
/// which the engine's answer must match
#[derive(Debug, Clone)]
pub(super) struct Reference {
    pub(super) model: Model,
    pub(super) prelude: Vec<Step>,
    pub(super) outcome: Result<(), Refusal>,
}

/// The workload as a state machine (proptest-state-machine): every id created first, with fields of its own, then
/// steps chosen from what the model holds, and one in eight chosen blind
pub(super) struct ModelMachine;

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
        let backup = Just(Step::Backup).boxed();
        let replace = Just(Step::ReplaceWith).boxed();
        let model = &state.model;
        let held_kind = |kind: &str| model.edges.iter().any(|(_, k, _)| k == kind);
        let held_field = |field: &str| {
            model
                .entities
                .iter()
                .any(|(id, e)| crate::engine::ids::type_of(id) == "Task" && e.fields.contains_key(field))
        };
        let (spare_field, spare_kind) = (held_field("spare"), held_kind("tags"));
        // A purge takes something only when the spare names were removed while data was held under them, and the writes
        // soon replace that data, so while it is held the declaration steps take a larger share: the toggle while the
        // names are declared, and a purge of what is held while they are removed
        let declaration_share = if spare_field || spare_kind { 10 } else { 4 };
        let declarations = if model.spares_removed {
            let (field, kind) = (1 + 4 * u32::from(spare_field), 1 + 4 * u32::from(spare_kind));
            prop_oneof![1 => Just(Step::Redeclare), field => Just(Step::PurgeField), kind => Just(Step::PurgeKind)]
                .boxed()
        } else {
            let toggle = 1 + 3 * u32::from(spare_field || spare_kind);
            prop_oneof![toggle => Just(Step::Redeclare), 1 => Just(Step::PurgeField), 1 => Just(Step::PurgeKind)]
                .boxed()
        };
        // A rename that moves nothing tests only its refusals, and the model holds few edges, so a rename is chosen
        // mostly from those that move something onto a spare name while there are any: the data a purge takes
        let moving_kinds: Vec<_> =
            RENAMES.iter().copied().filter(|&(from, to)| to == "tags" && held_kind(from)).collect();
        let moving_fields: Vec<_> =
            FIELD_RENAMES.iter().copied().filter(|&(from, to)| to == "spare" && held_field(from)).collect();
        let renames = |all: &'static [(&'static str, &'static str)], moving: Vec<(&'static str, &'static str)>| {
            if moving.is_empty() {
                prop::sample::select(all).boxed()
            } else {
                prop_oneof![1 => prop::sample::select(all), 3 => prop::sample::select(moving)].boxed()
            }
        };
        // The data is short-lived, the writes replacing it, so what a rename could move onto a spare name raises the
        // renames' share too
        let rename_share = if moving_kinds.is_empty() { 2 } else { 6 };
        let field_rename_share = if moving_fields.is_empty() { 2 } else { 4 };
        let rename =
            renames(&RENAMES[..], moving_kinds).prop_map(|(from, to)| Step::RenameRelation { from, to }).boxed();
        let field_rename =
            renames(&FIELD_RENAMES[..], moving_fields).prop_map(|(from, to)| Step::RenameField { from, to }).boxed();
        // The admin steps take a small share between them, so that the writes keep the share that makes each kind of
        // step complete often enough (`generated_workloads_complete_every_kind_of_step_and_make_every_refusal`)
        prop_oneof![
            16 => aimed(aimed_write(&state.model)),
            2 => blind(write()),
            4 => aimed(aimed_unit(&state.model, true)),
            2 => blind(Just(Step::RefuseNext).boxed()),
            2 => blind(rebuild),
            // A reset empties the model, which the steps of edges need some time to fill again, so it shares its slot
            2 => blind(prop_oneof![reset, Just(Step::RenameTypeAndBack)].boxed()),
            rename_share => blind(rename),
            field_rename_share => blind(field_rename),
            // A replacement completes only once a backup was taken, and a purge only while the spare names are removed
            2 => blind(prop_oneof![1 => backup, 2 => replace].boxed()),
            declaration_share => blind(declarations),
            2 => blind(onto_joined_ends(&state.model)),
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
pub(super) fn onto_joined_ends(m: &Model) -> BoxedStrategy<Step> {
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
pub(super) fn named(pool: &[&'static str], name: &str) -> &'static str {
    pool.iter().copied().find(|p| *p == name).expect("the model names only the pool's")
}

/// One write the model would take, chosen from what it holds: an id it has, or lacks for a create; an edge it has; a
/// key holding a list, or none, for a list's operations
pub(super) fn aimed_write(m: &Model) -> BoxedStrategy<Step> {
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
pub(super) fn aimed_unit(m: &Model, top: bool) -> BoxedStrategy<Step> {
    let inner = if top { prop_oneof![4 => aimed_write(m), 1 => aimed_unit(m, false)].boxed() } else { aimed_write(m) };
    (prop::collection::vec(inner, 1..6), prop::bool::weighted(0.25), any::<bool>())
        .prop_map(|(steps, throws, durable)| Step::Unit { steps, throws, durable })
        .boxed()
}

/// Every kind of step, as `kind_of` names it
pub(super) const STEP_KINDS: [&str; 33] = [
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
    "Backup",
    "ReplaceWith",
    "Redeclare",
    "PurgeField",
    "PurgeKind",
    "RenameTypeAndBack",
];

pub(super) fn kind_of(step: &Step) -> &'static str {
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
        Step::Backup => "Backup",
        Step::ReplaceWith => "ReplaceWith",
        Step::Redeclare => "Redeclare",
        Step::PurgeField => "PurgeField",
        Step::PurgeKind => "PurgeKind",
        Step::RenameTypeAndBack => "RenameTypeAndBack",
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
    const REFUSALS: [&str; 16] = [
        "id in use",
        "no such entity",
        "not a list",
        "self link",
        "no such relation",
        "duplicate edge",
        "link target missing at commit",
        "the unit threw",
        "the store refused the commit",
        "the spare field is declared",
        "the kind tags is declared",
        "the spare field isn't declared",
        "the kind tags isn't declared",
        "no backup to replace with",
        "a Task holds both names",
        "an edge of the new kind joins the ends",
    ];
    let mut runner = TestRunner::deterministic();
    let mut completed: BTreeMap<&str, usize> = BTreeMap::new();
    let mut refused: BTreeMap<&str, usize> = BTreeMap::new();
    let mut taking: BTreeMap<&str, usize> = BTreeMap::new();
    for _ in 0..200 {
        let (reference, transitions, _) =
            ModelMachine::sequential_strategy(10..60).new_tree(&mut runner).unwrap().current();
        let mut model = reference.model;
        for transition in transitions {
            // A purge that took nothing, a rename of a type with no entities and a replacement by what is there already
            // would each pass whatever the engine did
            let took = match transition.step {
                Step::PurgeField => model.entities.values().any(|e| e.fields.contains_key("spare")),
                Step::PurgeKind => model.edges.iter().any(|(_, kind, _)| kind == "tags"),
                Step::RenameTypeAndBack => model.entities.keys().any(|id| crate::engine::ids::type_of(id) == "Note"),
                Step::ReplaceWith => model.backup.as_deref().is_some_and(|saved| {
                    (&saved.entities, &saved.edges, &saved.keyed) != (&model.entities, &model.edges, &model.keyed)
                }),
                _ => false,
            };
            match model.step(&transition.step) {
                Ok(()) => {
                    *completed.entry(kind_of(&transition.step)).or_default() += 1;
                    if took {
                        *taking.entry(kind_of(&transition.step)).or_default() += 1;
                    }
                }
                Err(reason) => *refused.entry(reason).or_default() += 1,
            }
        }
    }
    for kind in STEP_KINDS {
        let n = completed.get(kind).copied().unwrap_or(0);
        assert!(n >= 20, "{kind} completed {n} times in 200 workloads: {completed:?}");
    }
    for kind in ["PurgeField", "PurgeKind", "RenameTypeAndBack", "ReplaceWith"] {
        let n = taking.get(kind).copied().unwrap_or(0);
        assert!(n >= 20, "{kind} had data to act on {n} times in 200 workloads: {taking:?}");
    }
    for reason in REFUSALS {
        assert!(refused.contains_key(reason), "no step was refused with {reason:?}: {refused:?}");
    }
}
