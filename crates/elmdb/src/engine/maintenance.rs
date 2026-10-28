//! A database's maintenance (`docs/architecture/engine.md`, Admin): backups, replacing or emptying its data, what it holds, and
//! loading data unchecked, as an import or a dump's load does.

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

use super::unit::{Read, Txn};
use super::{derive, entities, ids, keys, relations, values};

/// One unchecked load: an entity with its fields as they were, a relation under its own id, a sequence, an embedding, or
/// a field taken away
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
    /// A chunk's embedding, in the vector index `index` of the entity's type: its model, the hash of the text it embeds
    /// and its values. Loaded after its entity, whose fields file the chunk; an embedding of text the chunk no longer
    /// holds is stored and finds nothing, as a stale one does. No entity, and it is left out
    Embedding { id: String, index: String, chunk: u32, model: String, hash: String, vector: Vec<f32> },
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
            Load::Embedding { id, index, chunk, model, hash, vector } => {
                if entities::record(t, id)?.is_some() {
                    super::vector::restore(t, id, index, *chunk, model, hash, vector)?;
                }
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

pub(crate) fn primary_loads_of(r: &impl Read, mut each: impl FnMut(Load) -> Result<()>) -> Result<()> {
    let corrupt = |what: &str| Error::new(ErrorCode::Corrupted, format!("{what} doesn't read"));
    // Order identifiers rather than complete rows. Primary keys do not follow creation order, and derived type
    // ranges cannot establish what a dump must retain. One snapshot supplies both inventory and later row reads.
    let mut entities = derive::sorted::Scratch::new()?;
    r.try_visit(&keys::all_entities(), |key, bytes| {
        let Some(Part::String(id)) = decode_key(key)?.pop() else { return Err(corrupt("An entity's key")) };
        let record = values::decode(bytes)?;
        let items = record.as_array().ok_or_else(|| corrupt(&format!("{id}'s record")))?;
        let seq = items.first().and_then(Value::as_u64).ok_or_else(|| corrupt(&format!("{id}'s record")))?;
        let trashed = items.get(1).and_then(Value::as_bool).ok_or_else(|| corrupt(&format!("{id}'s record")))?;
        let mut order = seq.to_be_bytes().to_vec();
        order.extend_from_slice(id.as_bytes());
        entities.push(order, vec![u8::from(trashed)])
    })?;
    let mut entities = entities.finish()?;
    while let Some((order, flag)) = entities.next()? {
        let id = std::str::from_utf8(order.get(8..).ok_or_else(|| corrupt("A sorted entity id"))?)
            .map_err(|_| corrupt("A sorted entity id"))?
            .to_owned();
        let trashed = match flag.as_slice() {
            [0] => false,
            [1] => true,
            _ => return Err(corrupt("A sorted entity flag")),
        };
        let mut fields = BTreeMap::new();
        r.try_visit(&keys::values_of(&id), |key, bytes| {
            let Some(Part::String(field)) = decode_key(key)?.pop() else {
                return Err(corrupt("An entity's field key"));
            };
            fields.insert(field, values::decode(bytes)?);
            Ok(())
        })?;
        each(Load::Entity { id, fields, trashed })?;
    }
    drop(entities);
    let mut relations = derive::sorted::Scratch::new()?;
    r.try_visit(&keys::all_relations(), |key, bytes| {
        if !matches!(decode_key(key)?.last(), Some(Part::String(_))) {
            return Err(corrupt("A relation's key"));
        }
        let stored: relations::Stored = rmp_serde::from_slice(bytes).map_err(|_| corrupt("A relation's row"))?;
        let mut order = stored.seq.to_be_bytes().to_vec();
        order.extend_from_slice(key);
        relations.push(order, Vec::new())
    })?;
    let mut relations = relations.finish()?;
    while let Some((order, _)) = relations.next()? {
        let key = order.get(8..).ok_or_else(|| corrupt("A sorted relation key"))?;
        let Some(Part::String(id)) = decode_key(key)?.pop() else { return Err(corrupt("A relation's key")) };
        let bytes = r.get(key)?.ok_or_else(|| corrupt("A sorted relation's row"))?;
        let stored: relations::Stored = rmp_serde::from_slice(&bytes).map_err(|_| corrupt("A relation's row"))?;
        each(Load::Relation {
            id,
            kind: stored.kind,
            source: stored.source,
            target: stored.target,
            info: stored.info,
        })?;
    }
    drop(relations);
    r.try_visit(&Prefix::Vector.key().to_vec(), |key, bytes| {
        let mut parts = decode_key(key)?;
        let (Some(Part::Number(chunk)), Some(Part::String(id)), Some(Part::String(index))) =
            (parts.pop(), parts.pop(), parts.pop())
        else {
            return Err(corrupt("An embedding's key"));
        };
        let super::vector::Stored { model, hash, vector } = super::vector::decode_vector(bytes)?;
        each(Load::Embedding { id, index, chunk: chunk as u32, model, hash, vector })
    })?;
    r.try_visit(&Prefix::Sequence.key().to_vec(), |key, bytes| {
        let Some(Part::String(name)) = decode_key(key)?.pop() else { return Err(corrupt("A sequence's key")) };
        let next = values::decode(bytes)?.as_u64().ok_or_else(|| corrupt(&format!("sequence {name}")))?;
        each(Load::Sequence { name, next })
    })
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

    /// Atomically removes every entry but the database's own (its format and schema), with durable completion,
    /// the database staying open
    pub fn reset(&self) -> Result<()> {
        self.admin_unit_with_checkpoint(
            |t| {
                let kept = t.scan(&Prefix::Meta.key().to_vec())?;
                t.db.clear(t.txn)?;
                if let Some(cosine) = t.db.cosine {
                    cosine.clear(t.txn)?;
                }
                if let Some(euclidean) = t.db.euclidean {
                    euclidean.clear(t.txn)?;
                }
                for (key, value) in kept {
                    t.put(&key, &value)?;
                }
                super::settle_indexes_in(t)?;
                Ok(())
            },
            true,
        )
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
        #[cfg(windows)]
        options.map_size(crate::store::read_only_map_size(std::fs::metadata(source.join("data.mdb"))?.len(), None));
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
        // Retain one immutable source snapshot through retries, copying primary ranges directly into the write.
        // No source values are retained after each put, and derived source rows are never traversed.
        let result = self.admin_unit(|t| {
            let kept = t.scan(&Prefix::Meta.key().to_vec())?;
            t.db.clear(t.txn)?;
            for (key, value) in &kept {
                t.put(key, value)?;
            }
            for prefix in Prefix::ALL.into_iter().filter(|prefix| prefix.kind() == PrefixKind::Primary) {
                let (low, high) = crate::format::key::prefix_range(&prefix.key().to_vec());
                let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                for entry in from.range(&rtxn, &range)? {
                    let (key, value) = entry?;
                    t.put(key, value)?;
                }
            }
            derive::rebuild(t, None)?;
            super::settle_indexes_in(t)?;
            Ok(())
        });
        drop(rtxn);
        env.prepare_for_closing().wait();
        result
    }

    /// What the database holds: entities per type and field, relations per kind, entries per index, and its sizes
    pub fn stats(&self) -> Result<Stats> {
        let store = self.store()?;
        let schema = self.schema_guard();
        let mut stats = store.read(|txn, db| {
            schema.validate(txn, db)?;
            stats_of(&super::unit::Snapshot { txn, db, schema: &schema })
        })?;
        stats.file_bytes = store.file_size();
        stats.map_bytes = store.map_size() as u64;
        Ok(stats)
    }

    /// The database's primary data as the loads that make it again, read in one moment: its entities in the order they
    /// were made, each with every field it holds, then its relations in theirs, its embeddings, and its sequences last,
    /// so a load takes each in the place it had (`docs/architecture/format.md`, Dumps)
    pub fn primary_loads(&self, each: impl FnMut(Load) -> Result<()>) -> Result<()> {
        self.primary_snapshot(|_, snapshot| primary_loads_of(snapshot, each))
    }

    /// Captures declarations and primary data from one schema/snapshot pair. Release the schema guard after the
    /// LMDB snapshot starts, before export callbacks: their snapshot and captured schema stay valid independently.
    pub(crate) fn primary_snapshot<T>(
        &self,
        f: impl FnOnce(Option<&crate::schema::Schema>, &super::unit::Snapshot<'_, '_>) -> Result<T>,
    ) -> Result<T> {
        let store = self.store()?;
        let held = self.schema_guard();
        let schema = std::sync::Arc::clone(&held);
        let declared = self.schema();
        store.read(|txn, db| {
            schema.validate(txn, db)?;
            drop(held);
            f(declared.as_ref(), &super::unit::Snapshot { txn, db, schema: &schema })
        })
    }

    /// Applies loads in one durable unit without a write's checks, as an import or a dump's load does, and for specs that
    /// need data the checks would refuse
    pub fn bulk_load(&self, loads: &[Load]) -> Result<()> {
        self.admin_load(loads)
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;
    use crate::schema::Schema;
    use crate::store::Db;

    struct Streaming<'a, R>(&'a R);
    impl<R: Read> Read for Streaming<'_, R> {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            self.0.get(key)
        }
        fn visit(&self, prefix: &[u8], f: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
            self.0.visit(prefix, f)
        }
        fn visit_starting(&self, prefix: &[u8], f: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
            self.0.visit_starting(prefix, f)
        }
        fn visit_from(&self, prefix: &[u8], from: &[u8], f: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
            self.0.visit_from(prefix, from, f)
        }
        fn schema(&self) -> &Schema {
            self.0.schema()
        }
        fn raw(&self) -> (&heed::RoTxn<'_, heed::AnyTls>, Db) {
            self.0.raw()
        }
        fn scan(&self, _: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
            Err(Error::new(ErrorCode::Io, "export must not collect a primary range"))
        }
    }

    #[test]
    fn export_streams_primary_ranges_preserves_order_and_stops_on_consumer_error() {
        use crate::engine::tests::{open, write};
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        for i in (0..90).rev() {
            write(
                &db,
                vec![json!({"op":"create","type":"Task","id":format!("Task-{i:03}"),
                "fields":{"title":format!("row {i}"),"a":[i, {"nested":true}]}})],
            )
            .unwrap();
        }
        db.bulk_load(&[
            Load::Relation {
                id: "z".into(),
                kind: "refs".into(),
                source: "Task-089".into(),
                target: "Task-000".into(),
                info: None,
            },
            Load::Relation {
                id: "a".into(),
                kind: "refs".into(),
                source: "Task-000".into(),
                target: "Task-089".into(),
                info: Some(Value::Binary(vec![7; 4096])),
            },
        ])
        .unwrap();
        derive::sorted::with_budget(128, || {
            db.primary_snapshot(|_, r| {
                let r = Streaming(r);
                let mut entities = Vec::new();
                let mut relations = Vec::new();
                primary_loads_of(&r, |load| {
                    match load {
                        Load::Entity { id, .. } => entities.push(id),
                        Load::Relation { id, .. } => relations.push(id),
                        _ => {}
                    }
                    Ok(())
                })?;
                assert_eq!(entities, (0..90).rev().map(|i| format!("Task-{i:03}")).collect::<Vec<_>>());
                assert_eq!(relations, ["z", "a"]);
                let error = primary_loads_of(&r, |_| Err(Error::new(ErrorCode::Io, "consumer stopped"))).unwrap_err();
                assert_eq!(error.message, "consumer stopped");
                Ok(())
            })
        })
        .unwrap();
    }
}
