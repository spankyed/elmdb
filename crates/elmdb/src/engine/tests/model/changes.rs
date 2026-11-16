//! What each commit changed, as the engine reports it and as the model's states say, and what `changed_since` answers
//! of each type

use super::*;

/// What a reader of one type is told: whether one from the start is told the answer is incomplete, a reset or a
/// replacement having taken the type's entities away with no tombstone, or a rename of the type; the position a reader
/// from after the last of those starts at; and the position of the type's last rename, below which a reader is told so
#[derive(Default)]
pub(super) struct Reader {
    pub(super) replaced: bool,
    pub(super) replaced_at: Option<u64>,
    pub(super) renamed_at: Option<u64>,
}

impl Engine {
    /// A commit after a reset is where a reader from after the reset starts: the one before it
    pub(super) fn committed(&mut self, sequence: u64) {
        for reader in self.readers.values_mut() {
            if reader.replaced && reader.replaced_at.is_none() {
                reader.replaced_at = Some(sequence - 1);
            }
        }
    }
}

/// What `changed_since` answers from position zero: each entity still there with the sequence of its last change, and
/// those destroyed and not made again
#[derive(Default)]
pub(super) struct Since {
    pub(super) last: BTreeMap<String, u64>,
    pub(super) gone: BTreeSet<String>,
}

/// What a commit did to one entity, as the engine's change set names it: the fields are the ones the commit wrote, the
/// engine's own left out
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Change {
    pub(super) created: bool,
    pub(super) destroyed: bool,
    pub(super) trashed: bool,
    pub(super) restored: bool,
    /// The roles the entity holds differ: a grant or a revoke, which the engine names as its `role` field
    pub(super) roles: bool,
    pub(super) fields: BTreeSet<String>,
    /// Destroyed and made again within the commit: named with only what was done to the one made. Internal to the replay
    pub(super) replaced: bool,
}

/// The model's changes between two of its states, each entity the commit changed, with the fields whose values differ
pub(super) fn changes_between(before: &Model, after: &Model) -> BTreeMap<String, Change> {
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

pub(super) fn without_engine_fields(fields: BTreeSet<String>) -> BTreeSet<String> {
    fields.into_iter().filter(|f| !ENGINE_FIELDS.contains(&f.as_str())).collect()
}

/// The changes a commit's change set names, the engine's own fields left out and an entity that changed in no way the
/// model holds left out
pub(super) fn changes_of(changes: &ChangeSet) -> BTreeMap<String, Change> {
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
pub(super) fn effects_of(step: &Step, before: &Model, after: &Model) -> (BTreeMap<String, Change>, BTreeSet<String>) {
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
pub(super) fn effects_in(
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
pub(super) struct Rel {
    pub(super) kind: String,
    pub(super) source: String,
    pub(super) target: String,
    pub(super) linked: bool,
    pub(super) unlinked: bool,
    pub(super) changed: bool,
    /// The ends a moved relation had before the commit
    pub(super) moved_from: Option<(String, String)>,
    /// The key of a keyed edge, which tells two of the same ends apart. Internal to the replay: the engine's report has
    /// no key, so it is cleared before the comparison
    pub(super) n: Option<u8>,
    /// The key a rekey gave the edge, which is what the entry's key becomes. Internal to the replay
    pub(super) to_n: Option<u8>,
}

impl Rel {
    pub(super) fn new(kind: &str, source: &str, target: &str) -> Rel {
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
pub(super) fn relations_of(step: &Step, before: &Model) -> Vec<Rel> {
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

pub(super) fn rel_effects_in(current: &mut Model, step: &Step, rels: &mut Vec<Rel>) -> Result<(), Refusal> {
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
pub(super) fn edge_diff(before: &Model, after: &Model) -> Vec<Rel> {
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
pub(super) fn push_rel(rels: &mut Vec<Rel>, rel: Rel) {
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
pub(super) fn writes_of(step: &Step, previous: &Model) -> BTreeMap<String, Change> {
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
pub(super) fn written_ends(step: &Step) -> BTreeSet<String> {
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
pub(super) fn merge(into: &mut BTreeMap<String, Change>, from: BTreeMap<String, Change>) {
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
    pub(super) fn record(&mut self, sequence: u64, changes: &BTreeMap<String, Change>) {
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
pub(super) fn touched_ends(before: &Model, after: &Model) -> BTreeSet<String> {
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
pub(super) fn track(
    engine: &mut Engine,
    step: &Step,
    got: &Result<Option<ChangeSet>, crate::Error>,
    after: Model,
    quiet: u64,
) {
    // A rename or a purge commits no change set and reports no sequence: its commit is the one after the database's as
    // the step began. It stamps the entities whose fields it moved or took, or the ends of the edges it moved or took
    let holders = |field: &str| -> Vec<String> {
        engine
            .before
            .entities
            .iter()
            .filter(|(id, e)| crate::engine::ids::type_of(id) == "Task" && e.fields.contains_key(field))
            .map(|(id, _)| id.clone())
            .collect()
    };
    let ends = |kind: &str| -> Vec<String> {
        engine
            .before
            .edges
            .iter()
            .filter(|(_, k, _)| k.as_str() == kind)
            .flat_map(|(source, _, target)| [source.clone(), target.clone()])
            .collect()
    };
    let stamped = match (step, got) {
        (Step::RenameField { from, .. }, Ok(_)) => Some(holders(from)),
        (Step::PurgeField, Ok(_)) => Some(holders("spare")),
        (Step::RenameRelation { from, .. }, Ok(_)) => Some(ends(from)),
        (Step::PurgeKind, Ok(_)) => Some(ends("tags")),
        _ => None,
    };
    if let Some(stamped) = stamped {
        let sequence = quiet + 1;
        assert!(sequence > engine.sequence, "{step:?}: a commit's sequence only increases");
        engine.sequence = sequence;
        for id in stamped {
            if after.entities.contains_key(&id) {
                engine.since.gone.remove(&id);
                engine.since.last.insert(id, sequence);
            }
        }
        engine.committed(sequence);
        engine.before = after;
        assert_since(engine, step);
        return;
    }
    if let (Step::RenameTypeAndBack, Ok(_)) = (step, got)
        && engine.before.entities.keys().any(|id| crate::engine::ids::type_of(id) == "Note")
    {
        // Its commit takes `Note`'s entities away and brings them back, stamping both ends of each relation it rewrote,
        // each one with a `Note` end. A reader of `Note` from before it is told the answer is incomplete, and one from
        // it reads what changes after it; a reader of `Task` is told nothing new but the stamps of its entities
        let sequence = quiet + 1;
        assert!(sequence > engine.sequence, "{step:?}: a commit's sequence only increases");
        engine.sequence = sequence;
        engine.committed(sequence);
        let is_note = |id: &str| crate::engine::ids::type_of(id) == "Note";
        let rewritten = (after.edges.iter().map(|(source, _, target)| (source, target)))
            .chain(after.keyed.iter().map(|(source, target, _)| (source, target)))
            .filter(|(source, target)| is_note(source) || is_note(target));
        for (source, target) in rewritten {
            engine.since.last.insert(source.clone(), sequence);
            engine.since.last.insert(target.clone(), sequence);
        }
        engine.since.last.retain(|id, _| !is_note(id));
        engine.since.gone.retain(|id| !is_note(id));
        let note = engine.readers.get_mut("Note").expect("a reader of Note");
        *note = Reader { replaced: true, replaced_at: Some(sequence), renamed_at: Some(sequence) };
        engine.before = after;
        assert_since(engine, step);
        return;
    }
    if let (Step::Reset | Step::ReplaceWith, Ok(_)) = (step, got) {
        // A reset or a replacement commits no change set, and a reader from before it is told the answer is incomplete:
        // the entities it removed have no tombstone. The tracking starts again from the database as it is
        engine.before = after;
        engine.since = Since::default();
        for reader in engine.readers.values_mut() {
            reader.replaced = true;
            reader.replaced_at = None;
        }
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
            engine.committed(commit.sequence);
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

/// Asks `changed_since` of each type alone: from the start, from after the last reset, and from just before the type's
/// last rename
pub(super) fn assert_since(engine: &Engine, step: &Step) {
    for (entity_type, reader) in &engine.readers {
        let types = [entity_type.to_string()];
        let read = |sequence: u64| {
            let position = crate::engine::changed::Position { sequence, after: None };
            engine.db.changed_since(&types, &position, 1_000_000).expect("changed_since answers")
        };
        let from_start = read(0);
        if reader.replaced {
            assert!(
                !from_start.complete,
                "{step:?}: a reader of {entity_type} from before a reset is told it's incomplete"
            );
        } else {
            assert_answer(engine, step, entity_type, &from_start);
        }
        if let Some(sequence) = reader.replaced_at {
            assert_answer(engine, step, entity_type, &read(sequence));
        }
        if let Some(sequence) = reader.renamed_at {
            let before = read(sequence - 1);
            assert!(
                !before.complete,
                "{step:?}: a reader of {entity_type} from before its rename is told it's incomplete"
            );
        }
    }
}

/// What `changed_since` answers of a type, which must be its entities still there with the sequence of their last
/// change, and its entities destroyed and not made again, and everything
pub(super) fn assert_answer(engine: &Engine, step: &Step, entity_type: &str, got: &crate::engine::changed::Changed) {
    let of_type = |id: &String| crate::engine::ids::type_of(id) == entity_type;
    assert_eq!(got.ids.len(), got.sequences.len(), "each changed entity has the sequence of its last change");
    let ids: BTreeMap<String, u64> = got.ids.iter().cloned().zip(got.sequences.iter().copied()).collect();
    assert_eq!(got.ids.len(), ids.len(), "each changed entity is named once");
    let last: BTreeMap<String, u64> =
        engine.since.last.iter().filter(|(id, _)| of_type(id)).map(|(id, n)| (id.clone(), *n)).collect();
    assert_eq!(ids, last, "{step:?}: changed since, each {entity_type} still there and the commit of its last change");
    let destroyed: BTreeSet<String> = got.destroyed.iter().cloned().collect();
    assert_eq!(got.destroyed.len(), destroyed.len(), "each destroyed entity is named once");
    let gone: BTreeSet<String> = engine.since.gone.iter().filter(|id| of_type(id)).cloned().collect();
    assert_eq!(destroyed, gone, "changed since: each {entity_type} destroyed and not made again");
    assert!(got.done && got.complete, "changed since answers everything of {entity_type}: {got:?}");
}
