//! The reference model: what each step does to the entities, edges and keyed edges it holds, kept as plainly as a
//! model can be, and what it refuses

use super::*;

/// One entity as the model holds it
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Entity {
    pub(super) seq: u64,
    pub(super) fields: BTreeMap<String, Json>,
    pub(super) trashed: bool,
    pub(super) roles: Vec<String>,
}

/// The data as the model holds it
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Model {
    /// The data as the last `Backup` copied it, which `ReplaceWith` takes the database back to
    pub(super) backup: Option<Box<Saved>>,
    /// The schema lacks the spare names (`harness_schema`), so data under them waits to be purged
    pub(super) spares_removed: bool,
    pub(super) entities: BTreeMap<String, Entity>,
    /// Each edge by its ends: a kind with no key joins two entities by one edge at most
    pub(super) edges: BTreeSet<(String, String, String)>,
    /// Each `cites` edge by its ends and key: one a pair for each value of `n`
    pub(super) keyed: BTreeSet<(String, String, u8)>,
    /// The edges a unit linked, whose targets must exist when it commits
    pub(super) pending: Vec<(String, String, String)>,
    pub(super) next_seq: u64,
    pub(super) refuse_next: bool,
}

/// The primary data a backup holds
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Saved {
    pub(super) entities: BTreeMap<String, Entity>,
    pub(super) edges: BTreeSet<(String, String, String)>,
    pub(super) keyed: BTreeSet<(String, String, u8)>,
}

/// Why the model refused a step; the engine must refuse it too
pub(super) type Refusal = &'static str;

/// The code the engine refuses with for each reason the model refuses: a refusal for another reason than the model's
/// is one the engine got wrong, though it refused
pub(super) fn code_of(reason: Refusal) -> crate::ErrorCode {
    use crate::ErrorCode as Code;
    match reason {
        "id in use" => Code::IdInUse,
        "no such entity" | "link target missing at commit" => Code::NoSuchEntity,
        "not a list" => Code::NotAList,
        "self link" => Code::SelfLink,
        "no such relation" => Code::NoSuchRelation,
        "duplicate edge" | "an edge of the new kind joins the ends" => Code::DuplicateEdge,
        // The harness's own throw, and the failure an armed refusal injects at the commit
        "the unit threw" | "the store refused the commit" => Code::Io,
        "a Task holds both names"
        | "the spare field is declared"
        | "the kind tags is declared"
        | "the spare field isn't declared"
        | "the kind tags isn't declared" => Code::SchemaChangeRefused,
        "no backup to replace with" => Code::NoDatabase,
        other => panic!("the model refuses with {other:?}, which names no code"),
    }
}

impl Model {
    pub(super) fn exists(&self, id: &str) -> Result<(), Refusal> {
        if self.entities.contains_key(id) { Ok(()) } else { Err("no such entity") }
    }

    /// Applies one write, as inside a unit
    pub(super) fn apply(&mut self, step: &Step) -> Result<(), Refusal> {
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
            Step::RefuseNext
            | Step::Reset
            | Step::RenameRelation { .. }
            | Step::RenameField { .. }
            | Step::Backup
            | Step::ReplaceWith
            | Step::Redeclare
            | Step::PurgeField
            | Step::PurgeKind
            | Step::RenameTypeAndBack => unreachable!("only at the top"),
        }
        Ok(())
    }

    /// The deferred check: every edge the unit linked that still exists has its target
    pub(super) fn check_pending(&self) -> Result<(), Refusal> {
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
    pub(super) fn step(&mut self, step: &Step) -> Result<(), Refusal> {
        if let Step::RefuseNext = step {
            self.refuse_next = true;
            return Ok(());
        }
        if let Step::Redeclare = step {
            if std::mem::take(&mut self.refuse_next) {
                return Err("the store refused the commit");
            }
            self.spares_removed = !self.spares_removed;
            return Ok(());
        }
        if let Step::RenameTypeAndBack = step {
            // Nothing that `Memo`'s declaration, `Note`'s own, could refuse: the renames commit, or the store refuses it
            if std::mem::take(&mut self.refuse_next) {
                return Err("the store refused the commit");
            }
            self.pending.clear();
            return Ok(());
        }
        if let Step::PurgeField = step {
            // Refused before any commit while the field is declared, so an armed refusal waits for the next commit
            if !self.spares_removed {
                return Err("the spare field is declared");
            }
            if std::mem::take(&mut self.refuse_next) {
                return Err("the store refused the commit");
            }
            for (id, entity) in self.entities.iter_mut() {
                if crate::engine::ids::type_of(id) == "Task" {
                    entity.fields.remove("spare");
                }
            }
            return Ok(());
        }
        if let Step::PurgeKind = step {
            if !self.spares_removed {
                return Err("the kind tags is declared");
            }
            if std::mem::take(&mut self.refuse_next) {
                return Err("the store refused the commit");
            }
            self.edges.retain(|(_, kind, _)| kind != "tags");
            return Ok(());
        }
        if let Step::RenameField { from, to } = step {
            // A rename to a name the schema doesn't declare is refused before the commit
            if *to == "spare" && self.spares_removed {
                return Err("the spare field isn't declared");
            }
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
            if *to == "tags" && self.spares_removed {
                return Err("the kind tags isn't declared");
            }
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
        if let Step::Backup = step {
            // A copy, which commits nothing: an armed refusal waits for the next commit
            let saved = Saved { entities: self.entities.clone(), edges: self.edges.clone(), keyed: self.keyed.clone() };
            self.backup = Some(Box::new(saved));
            return Ok(());
        }
        if let Step::ReplaceWith = step {
            // With no backup the engine finds no database to replace with, before any commit
            let Some(saved) = self.backup.clone() else { return Err("no backup to replace with") };
            if std::mem::take(&mut self.refuse_next) {
                return Err("the store refused the commit");
            }
            self.entities = saved.entities;
            self.edges = saved.edges;
            self.keyed = saved.keyed;
            self.pending.clear();
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
