//! What a commit changed (`docs/architecture/engine.md`, Change sets and watch): the events a unit logs as it writes, gathered into
//! one change set when it commits. A savepoint rolled back takes its events with it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// One thing a unit did
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Created {
        id: String,
    },
    Field {
        id: String,
        field: String,
    },
    Trashed {
        id: String,
    },
    Restored {
        id: String,
    },
    Destroyed {
        id: String,
    },
    Linked {
        relation: String,
        kind: String,
        source: String,
        target: String,
    },
    Unlinked {
        relation: String,
        kind: String,
        source: String,
        target: String,
    },
    /// A relation's ends, info or weight replaced: its ends as they are now, and with `moved_from` the ends it had when
    /// this change moved it
    RelationChanged {
        relation: String,
        kind: String,
        source: String,
        target: String,
        moved_from: Option<Ends>,
    },
}

/// The two entities a relation joins
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Ends {
    pub source: String,
    pub target: String,
}

/// What one commit changed
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ChangeSet {
    /// Per entity, what happened to it
    pub entities: BTreeMap<String, EntityChange>,
    /// Per relation, what happened to it
    pub relations: BTreeMap<String, RelationChange>,
    /// The types whose entities changed
    pub types: BTreeSet<String>,
    /// The relation kinds whose edges changed
    pub kinds: BTreeSet<String>,
    /// The commit's sequence: a number that only increases for as long as the database exists
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub sequence: u64,
    /// The tokens of the tracked reads whose keys and ranges the commit wrote into
    pub touched: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct EntityChange {
    #[serde(rename = "type")]
    pub entity_type: String,
    pub created: bool,
    /// The entity is gone as the commit ends: one destroyed and made again under its id in the commit is `created`
    /// alone, and one made and destroyed in it is both
    pub destroyed: bool,
    pub trashed: bool,
    pub restored: bool,
    /// The fields written, the engine's own (`updatedAt`) included
    pub fields: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RelationChange {
    pub kind: String,
    pub source: String,
    pub target: String,
    pub linked: bool,
    pub unlinked: bool,
    pub changed: bool,
    /// The ends the relation had as the commit began, when the commit moved it to others: the entities that lost the
    /// edge, which `source` and `target`, the ends it has now, don't name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub moved_from: Option<Ends>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.relations.is_empty()
    }

    /// The change set of these events, in the order they happened
    pub fn of(events: &[Event]) -> Self {
        let mut set = ChangeSet::default();
        let type_of = |id: &str| super::ids::type_of(id).to_owned();
        for event in events {
            match event {
                Event::Created { id }
                | Event::Field { id, .. }
                | Event::Trashed { id }
                | Event::Restored { id }
                | Event::Destroyed { id } => {
                    let entry = set
                        .entities
                        .entry(id.clone())
                        .or_insert_with(|| EntityChange { entity_type: type_of(id), ..Default::default() });
                    match event {
                        Event::Created { .. } => {
                            // Made again after the commit destroyed it: a reader that drops what `destroyed` names
                            // would drop an entity that exists
                            entry.created = true;
                            entry.destroyed = false;
                        }
                        Event::Field { field, .. } => {
                            entry.fields.insert(field.clone());
                        }
                        Event::Trashed { .. } => entry.trashed = true,
                        Event::Restored { .. } => entry.restored = true,
                        Event::Destroyed { .. } => entry.destroyed = true,
                        _ => unreachable!(),
                    }
                    set.types.insert(type_of(id));
                }
                Event::Linked { relation, kind, source, target }
                | Event::Unlinked { relation, kind, source, target }
                | Event::RelationChanged { relation, kind, source, target, .. } => {
                    let entry = set.relations.entry(relation.clone()).or_insert_with(|| RelationChange {
                        kind: kind.clone(),
                        source: source.clone(),
                        target: target.clone(),
                        linked: false,
                        unlinked: false,
                        changed: false,
                        moved_from: None,
                    });
                    entry.source = source.clone();
                    entry.target = target.clone();
                    match event {
                        Event::Linked { .. } => entry.linked = true,
                        Event::Unlinked { .. } => entry.unlinked = true,
                        Event::RelationChanged { moved_from, .. } => {
                            entry.changed = true;
                            // The first move's: the ends a later one left are ends this commit gave it
                            if entry.moved_from.is_none() {
                                entry.moved_from.clone_from(moved_from);
                            }
                        }
                        _ => unreachable!(),
                    }
                    set.kinds.insert(kind.clone());
                }
            }
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entity_destroyed_and_made_again_in_a_commit_is_created_and_one_made_and_destroyed_is_both() {
        let created = || Event::Created { id: "Task-1".into() };
        let destroyed = || Event::Destroyed { id: "Task-1".into() };
        let of = |events: &[Event]| {
            let change = ChangeSet::of(events).entities["Task-1"].clone();
            (change.created, change.destroyed)
        };
        assert_eq!(of(&[destroyed(), created()]), (true, false));
        assert_eq!(of(&[created(), destroyed()]), (true, true));
        assert_eq!(of(&[destroyed()]), (false, true));
        assert_eq!(of(&[created(), destroyed(), created()]), (true, false));
    }
}
