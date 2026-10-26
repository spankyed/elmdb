//! `elmdb upgrade`: an apack format 4 database, dumped by `tools/lmdb-js-export`, as an elmdb database (`docs/architecture/format.md`,
//! Moving apack's data). Format 4's keys are tuples in the same codec, its strings holding no NUL; its primary keys map
//! onto elmdb's prefixes, its sequences carry over, and derived data is rebuilt rather than copied.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;

use rmpv::Value;
use serde::Serialize;

use crate::database::{Database, OpenOptions};
use crate::engine::maintenance::Load;
use crate::error::{Error, ErrorCode, Result};
use crate::format::key::{Part, decode as decode_key};
use crate::schema::Schema;

/// What an upgrade read and wrote
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeReport {
    pub entries: u64,
    pub entities: u64,
    pub attributes: u64,
    pub relations: u64,
    pub sequences: u64,
    /// The keys left behind, by their first part: format 4's derived keys, which are rebuilt
    pub skipped: BTreeMap<String, u64>,
}

fn invalid(why: impl Into<String>) -> Error {
    Error::new(ErrorCode::WrongFormat, format!("The dump isn't a format 4 dump elmdb reads: {}", why.into()))
}

/// The entries of a dump, in key order
fn entries(path: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut file = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut head = [0u8; 12];
    file.read_exact(&mut head)?;
    if &head[..8] != b"ELMDBF4\0" || u32::from_le_bytes(head[8..].try_into().expect("4 bytes")) != 1 {
        return Err(invalid("its header isn't ELMDBF4, version 1"));
    }
    let mut out = Vec::new();
    let mut len = [0u8; 4];
    loop {
        file.read_exact(&mut len)?;
        let n = u32::from_le_bytes(len);
        if n == u32::MAX {
            break;
        }
        let key = read_bytes(&mut file, n)?;
        file.read_exact(&mut len)?;
        let value = read_bytes(&mut file, u32::from_le_bytes(len))?;
        out.push((key, value));
    }
    let mut count = [0u8; 8];
    file.read_exact(&mut count)?;
    if u64::from_le_bytes(count) != out.len() as u64 {
        return Err(invalid(format!("it records {} entries and holds {}", u64::from_le_bytes(count), out.len())));
    }
    Ok(out)
}

/// `n` bytes of the dump, read as they arrive rather than allocated up front, so a damaged length fails as the dump
/// ending rather than as an allocation of up to 4 GiB
fn read_bytes(file: &mut impl std::io::Read, n: u32) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(u64::from(n)).read_to_end(&mut bytes)?;
    if bytes.len() != n as usize {
        return Err(invalid(format!("it ends inside an entry {n} bytes long")));
    }
    Ok(bytes)
}

/// Format 4's name for a sequence as elmdb names it: a short code's `<Type>:<PREFIX>` is `shortCode:<Type>`
fn sequence_name(name: &str) -> String {
    if name.starts_with("label:") {
        return name.to_owned();
    }
    match name.split_once(':') {
        Some((entity_type, _prefix)) => format!("shortCode:{entity_type}"),
        None => name.to_owned(),
    }
}

/// msgpackr's encoding of JavaScript's `undefined`: extension type 0, which format 4's own rows hold for a field left
/// unset (a relation's `info`)
fn is_undefined(value: &Value) -> bool {
    matches!(value, Value::Ext(0, _))
}

/// One of format 4's own rows, an entity's record or a relation's: msgpack, read without the value spec, since they
/// may hold `undefined`
fn row(bytes: &[u8], what: &str) -> Result<Value> {
    rmpv::decode::read_value(&mut &bytes[..]).map_err(|e| invalid(format!("{what} doesn't decode: {e}")))
}

fn map_field<'v>(value: &'v Value, name: &str) -> Option<&'v Value> {
    value.as_map()?.iter().find(|(k, _)| k.as_str() == Some(name)).map(|(_, v)| v)
}

/// Reads a dump into a new database at `target`, held to `schema` when one is given
pub fn upgrade(dump: &Path, target: &Path, schema: Option<Schema>) -> Result<UpgradeReport> {
    if target.join("data.mdb").exists() {
        return Err(Error::new(
            ErrorCode::IdInUse,
            format!("{} holds a database already; upgrade writes a new one", target.display()),
        ));
    }
    let mut report = UpgradeReport::default();
    // Entities and their fields first, then relations, so each relation's ends exist as it is filed
    let mut records: BTreeMap<String, (u64, bool)> = BTreeMap::new();
    let mut fields: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let mut relations: Vec<(u64, Load)> = Vec::new();
    let mut sequences: Vec<Load> = Vec::new();
    for (key, value) in entries(dump)? {
        report.entries += 1;
        let parts = decode_key(&key).map_err(|_| invalid(format!("a key doesn't decode: {key:02x?}")))?;
        let tag = match parts.first() {
            Some(Part::String(tag)) => tag.as_str(),
            _ => return Err(invalid("a key doesn't start with its tag")),
        };
        match (tag, &parts[1..]) {
            ("v", [Part::String(id), Part::String(field)]) => {
                let decoded = crate::format::value::decode(&value)
                    .map_err(|e| invalid(format!("{id}.{field}: {}", e.message)))?;
                fields.entry(id.clone()).or_default().insert(field.clone(), decoded);
                report.attributes += 1;
            }
            ("i", [Part::String(id)]) => {
                let record = row(&value, &format!("{id}'s record"))?;
                let seq = map_field(&record, "seq")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid(format!("{id}'s record holds no seq")))?;
                let trashed = map_field(&record, "trashed").and_then(Value::as_bool).unwrap_or(false);
                records.insert(id.clone(), (seq, trashed));
                report.entities += 1;
            }
            ("r", [Part::String(id)]) => {
                let stored = row(&value, &format!("relation {id}"))?;
                let text = |name: &str| map_field(&stored, name).and_then(Value::as_str).map(str::to_owned);
                let seq = map_field(&stored, "seq").and_then(Value::as_u64).unwrap_or(0);
                let info = map_field(&stored, "info").filter(|v| !v.is_nil() && !is_undefined(v)).cloned();
                if let Some(info) = &info {
                    crate::format::value::decode(&crate::engine::encode_value(info))
                        .map_err(|e| invalid(format!("relation {id}'s info: {}", e.message)))?;
                }
                let (Some(kind), Some(source), Some(target)) = (text("kind"), text("source"), text("target")) else {
                    return Err(invalid(format!("relation {id}'s row lacks its kind or an end")));
                };
                relations.push((seq, Load::Relation { id: id.clone(), kind, source, target, info }));
                report.relations += 1;
            }
            ("s", [Part::String(name)]) => {
                let next = crate::format::value::decode(&value).ok().and_then(|v| v.as_u64()).unwrap_or(1);
                sequences.push(Load::Sequence { name: sequence_name(name), next });
                report.sequences += 1;
            }
            ("m", [Part::String(name)]) if name == "seq" => {
                let next = crate::format::value::decode(&value).ok().and_then(|v| v.as_u64()).unwrap_or(0);
                sequences.push(Load::Sequence { name: "seq".to_owned(), next });
                report.sequences += 1;
            }
            _ => *report.skipped.entry(tag.to_owned()).or_default() += 1,
        }
    }
    // A failed upgrade removes what it wrote, so it can be run again
    let written = Written(Some(target.to_owned()));
    let db = Database::open(target, OpenOptions { schema, ..Default::default() })?;
    // Entities in the order they were created, so each keeps its place
    let mut ordered: Vec<(String, (u64, bool))> = records.into_iter().collect();
    ordered.sort_by_key(|(_, (seq, _))| *seq);
    let mut loads: Vec<Load> = ordered
        .into_iter()
        .map(|(id, (_, trashed))| {
            let entity_fields = fields.remove(&id).unwrap_or_default();
            Load::Entity { id, fields: entity_fields, trashed }
        })
        .collect();
    // Fields of an id with no record have no entity to load into: format 4 left them behind
    let orphans: u64 = fields.values().map(|f| f.len() as u64).sum();
    if orphans > 0 {
        report.attributes -= orphans;
        *report.skipped.entry("v".to_owned()).or_default() += orphans;
    }
    relations.sort_by_key(|(seq, _)| *seq);
    loads.extend(relations.into_iter().map(|(_, load)| load));
    // The sequences last, so the store's own takes the place format 4 left it at, past every entity and relation
    loads.extend(sequences);
    let loaded = db.bulk_load(&loads).and_then(|_| db.rebuild(None));
    let closed = db.close();
    loaded?;
    closed?;
    written.keep();
    Ok(report)
}

/// The files an upgrade writes into its target, removed unless it finishes
struct Written(Option<std::path::PathBuf>);

impl Written {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for Written {
    fn drop(&mut self) {
        if let Some(target) = self.0.take() {
            for file in ["data.mdb", "lock.mdb"] {
                let _ = std::fs::remove_file(target.join(file));
            }
        }
    }
}
