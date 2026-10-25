//! What a commit changed (`docs/architecture/engine.md`, Change sets and watch): the events a unit logs as it writes, gathered into
//! one change set when it commits. A savepoint rolled back takes its events with it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// One thing a unit did
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Created { id: String },
    Field { id: String, field: String },
    Trashed { id: String },
    Restored { id: String },
    Destroyed { id: String },
    Linked { relation: String, kind: String, source: String, target: String },
    Unlinked { relation: String, kind: String, source: String, target: String },
    RelationChanged { relation: String, kind: String, source: String, target: String },
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
                        Event::Created { .. } => entry.created = true,
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
                | Event::RelationChanged { relation, kind, source, target } => {
                    let entry = set.relations.entry(relation.clone()).or_insert_with(|| RelationChange {
                        kind: kind.clone(),
                        source: source.clone(),
                        target: target.clone(),
                        linked: false,
                        unlinked: false,
                        changed: false,
                    });
                    entry.source = source.clone();
                    entry.target = target.clone();
                    match event {
                        Event::Linked { .. } => entry.linked = true,
                        Event::Unlinked { .. } => entry.unlinked = true,
                        _ => entry.changed = true,
                    }
                    set.kinds.insert(kind.clone());
                }
            }
        }
        set
    }
}
