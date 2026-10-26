//! A database's maintenance (`docs/architecture/engine.md`, Admin): backups, replacing or emptying its data, what it holds, and
//! loading data unchecked, as an import or an upgrade does.

use std::collections::BTreeMap;
use std::path::Path;

use heed::types::Bytes;
use heed::{CommitOrAbort, EnvFlags, EnvOpenOptions};
use rmpv::Value;
use serde::{Deserialize, Serialize};

use crate::database::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::format::key::{Part, decode as decode_key};
use crate::format::{self, Prefix, PrefixKind, meta};

use super::unit::{Read, Txn, UnitState};
use super::{derive, entities, ids, keys, relations, values};

/// One unchecked load: an entity with its fields as they were, a relation under its own id, or a sequence
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(tag = "load", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Load {
    Entity {
        id: String,
        #[cfg_attr(feature = "codegen", ts(type = "Record<string, unknown>"))]
        fields: BTreeMap<String, Value>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        trashed: bool,
    },
    Relation {
        id: String,
        kind: String,
        source: String,
        target: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(type = "unknown", optional))]
        info: Option<Value>,
    },
    /// A numbering sequence's next number: `shortCode:<Type>`, `label:<Type>:<base>`, or the store's `seq`
    Sequence { name: String, next: u64 },
    /// A field removed from an entity, any field the engine writes included: removing `trashedAt` takes it out of the
    /// trash. An entity without the field, or no entity, is left as it is
    Unset { id: String, field: String },
}

/// What a database holds
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    /// Per type, its live and trashed entities
    pub types: BTreeMap<String, TypeStats>,
    /// Per relation kind, its relations
    pub relations: BTreeMap<String, u64>,
    /// Per type and index, its entries
    pub indexes: BTreeMap<String, u64>,
    pub entries: u64,
    pub file_bytes: u64,
    pub map_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct TypeStats {
    pub live: u64,
    pub trashed: u64,
    /// Per field, how many entities hold it and the bytes of their values
    pub fields: BTreeMap<String, FieldStats>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct FieldStats {
    pub entities: u64,
    pub bytes: u64,
}

/// Applies loads in one unit without a write's checks: no schema, no refusals, the ids and fields as given. The derived
/// keys an entity's and a relation's writes keep are kept
pub fn load(t: &mut Txn<'_, '_>, loads: &[Load]) -> Result<()> {
    for item in loads {
        match item {
            Load::Entity { id, fields, trashed } => {
                if entities::record(t, id)?.is_none() {
                    entities::insert(t, id)?;
                }
                for (name, value) in fields {
                    entities::put_field(t, id, name, value)?;
                }
                if *trashed && entities::get(t, id, entities::field::TRASHED_AT)?.is_none() {
                    entities::put_field(t, id, entities::field::TRASHED_AT, &Value::from(0))?;
                }
            }
            Load::Relation { id, kind, source, target, info } => {
                if relations::get(t, id)?.is_some() {
                    relations::remove(t, id)?;
                }
                let seq = entities::next_seq(t)?;
                let stored = relations::Stored {
                    kind: kind.clone(),
                    source: source.clone(),
                    target: target.clone(),
                    info: info.clone(),
                    seq,
                };
                relations::restore(t, id, &stored)?;
            }
            Load::Sequence { name, next } => {
                // A numbering never goes back: the entities loaded before it took numbers from it already
                let held = t.get(&keys::sequence(name))?.and_then(|b| values::decode(&b).ok()).and_then(|v| v.as_u64());
                let next = held.map_or(*next, |held| held.max(*next));
                t.put(&keys::sequence(name), &values::encode(&next.into()))?;
            }
            Load::Unset { id, field } => {
                if entities::record(t, id)?.is_some() {
                    entities::delete_field(t, id, field)?;
                }
            }
        }
    }
    Ok(())
}

fn stats_of(r: &impl Read) -> Result<Stats> {
    let mut stats = Stats::default();
    for (key, bytes) in r.scan(&keys::all_entities())? {
        let id = match decode_key(&key)?.pop() {
            Some(Part::String(id)) => id,
            _ => continue,
        };
        let trashed =
            values::decode(&bytes)?.as_array().and_then(|a| a.get(1)).and_then(Value::as_bool).unwrap_or(false);
        let entry = stats.types.entry(ids::type_of(&id).to_owned()).or_default();
        if trashed { entry.trashed += 1 } else { entry.live += 1 }
    }
    for (key, value) in r.scan(&keys::all_values())? {
        let parts = decode_key(&key)?;
        if let (Some(Part::String(id)), Some(Part::String(field))) = (parts.get(1), parts.get(2)) {
            let f =
                stats.types.entry(ids::type_of(id).to_owned()).or_default().fields.entry(field.clone()).or_default();
            f.entities += 1;
            f.bytes += value.len() as u64;
        }
    }
    for (_, bytes) in r.scan(&keys::all_relations())? {
        let stored: relations::Stored = rmp_serde::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::Corrupted, format!("A relation's row doesn't read: {e}")))?;
        *stats.relations.entry(stored.kind).or_default() += 1;
    }
    for (key, _) in r.scan(&Prefix::Index.key().to_vec())? {
        let parts = decode_key(&key)?;
        if let (Some(Part::String(t)), Some(Part::String(i))) = (parts.get(1), parts.get(2)) {
            *stats.indexes.entry(format!("{t}.{i}")).or_default() += 1;
        }
    }
    for prefix in Prefix::ALL {
        let mut n = 0;
        r.visit(&prefix.key().to_vec(), |_, _| {
            n += 1;
            true
        })?;
        stats.entries += n;
    }
    Ok(stats)
}

impl Database {
    /// A compacted copy of one moment of the database into `target`, which opens as a database of its own
    pub fn backup(&self, target: impl AsRef<Path>) -> Result<()> {
        self.store()?.copy_to(target.as_ref())
    }

    /// Every entry removed but the database's own (its format and schema), in one durable unit, the database staying
    /// open
    pub fn reset(&self) -> Result<()> {
        let store = self.store()?;
        store.write(
            |txn, db| {
                let kept: Vec<(Vec<u8>, Vec<u8>)> = {
                    let (low, high) = format::key::prefix_range(&Prefix::Meta.key().to_vec());
                    let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                    db.range(txn, &range)?
                        .map(|e| e.map(|(k, v)| (k.to_vec(), v.to_vec())))
                        .collect::<heed::Result<_>>()?
                };
                db.clear(txn)?;
                // The vector graphs go with the keys they are derived from
                if let Some(cosine) = db.cosine {
                    cosine.clear(txn)?;
                }
                if let Some(euclidean) = db.euclidean {
                    euclidean.clear(txn)?;
                }
                for (k, v) in kept {
                    db.put(txn, &k, &v)?;
                }
                Ok(())
            },
            true,
        )?;
        super::settle_indexes(&store, &self.engine_schema())?;
        self.touched_everything(&store)
    }

    /// Every entry replaced by another database's primary data, and derived data rebuilt from it under this database's
    /// schema, in one durable unit, the database staying open. The other database must be an elmdb database in this
    /// version's format
    pub fn replace_with(&self, source: impl AsRef<Path>) -> Result<()> {
        let source = source.as_ref();
        if !source.join("data.mdb").is_file() {
            return Err(Error::new(ErrorCode::NoDatabase, format!("No database at {}", source.display())));
        }
        let mut options = EnvOpenOptions::new().read_txn_without_tls();
        options.max_dbs(8);
        // SAFETY: the source is another directory than this database's, opened read-only for this call alone
        unsafe { options.flags(EnvFlags::READ_ONLY) };
        let env = unsafe { options.open(source) }?;
        let rtxn = env.read_txn()?;
        let from = env
            .open_databases_and_commit(rtxn, |opener, rtxn| -> heed::Result<_> {
                Ok(CommitOrAbort::<_, ()>::Commit(opener.open_database::<Bytes, Bytes>(rtxn, Some(format::DB_NAME))?))
            })?
            .unwrap_commit()
            .ok_or_else(|| {
                Error::new(ErrorCode::NotADatabase, format!("{} holds no elmdb database", source.display()))
            })?;
        let rtxn = env.read_txn()?;
        let stamped = from.get(&rtxn, &crate::store::meta_key(meta::FORMAT))?.map(<[u8]>::to_vec);
        if stamped.as_deref() != Some(&format::FORMAT_VERSION.to_be_bytes()[..]) {
            return Err(Error::new(
                ErrorCode::WrongFormat,
                format!("{} isn't a database in format {}", source.display(), format::FORMAT_VERSION),
            ));
        }
        let primary: Vec<(Vec<u8>, Vec<u8>)> = from
            .iter(&rtxn)?
            .collect::<heed::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(k, _)| {
                decode_key(k)
                    .ok()
                    .and_then(|p| match p.first() {
                        Some(Part::String(tag)) => Prefix::from_tag(tag),
                        _ => None,
                    })
                    .is_some_and(|p| p.kind() == PrefixKind::Primary)
            })
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();
        drop(rtxn);
        env.prepare_for_closing().wait();
        // Held for the replace, so a schema change waits rather than changing the schema the data is rebuilt under
        let schema = self.writing_schema();
        let store = self.store()?;
        store.write(
            |txn, db| {
                let kept: Vec<(Vec<u8>, Vec<u8>)> = {
                    let (low, high) = format::key::prefix_range(&Prefix::Meta.key().to_vec());
                    let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                    db.range(txn, &range)?
                        .map(|e| e.map(|(k, v)| (k.to_vec(), v.to_vec())))
                        .collect::<heed::Result<_>>()?
                };
                db.clear(txn)?;
                for (k, v) in kept.iter().chain(primary.iter()) {
                    db.put(txn, k, v)?;
                }
                let mut state = UnitState::default();
                derive::rebuild(&mut Txn { txn, db, state: &mut state, schema: &schema, now: 0.0 }, None)
            },
            true,
        )?;
        // As a reset does: an index of a type the data left empty is built, there being nothing to build
        super::settle_indexes(&store, &schema)?;
        drop(schema);
        self.touched_everything(&store)
    }

    /// What the database holds: entities per type and field, relations per kind, entries per index, and its sizes
    pub fn stats(&self) -> Result<Stats> {
        let store = self.store()?;
        let schema = self.engine_schema();
        let mut stats = store.read(|txn, db| stats_of(&super::unit::Snapshot { txn, db, schema: &schema }))?;
        stats.file_bytes = store.file_size();
        stats.map_bytes = store.map_size() as u64;
        Ok(stats)
    }

    /// Applies loads in one durable unit without a write's checks, as an import or an upgrade does, and for specs that
    /// need data the checks would refuse
    pub fn bulk_load(&self, loads: &[Load]) -> Result<()> {
        self.admin_load(loads)
    }
}
