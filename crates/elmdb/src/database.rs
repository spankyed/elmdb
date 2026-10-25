//! A database: one directory holding one LMDB environment, opened at most once per process, with its schema
//! (`docs/engine.md`, Opening).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{Error, ErrorCode, Result};
use crate::format::meta;
use crate::schema::{Effect, SCHEMA_LANGUAGE, Schema, SchemaChange, diff};
use crate::store::sync::Durable;
use crate::store::{Store, StoreOptions, put_meta};

/// How a database is opened
#[derive(Debug, Clone)]
pub struct OpenOptions {
    /// Open existing files without writing: every write is refused, and the stored schema is used
    pub read_only: bool,
    /// The schema to hold the database to. Opening with one that differs from the stored schema changes it
    /// (`docs/schema.md`, Changing a schema); `None` keeps the stored one
    pub schema: Option<Schema>,
    /// Group commit's syncs; off, nothing is synced and `durable()` resolves at once, for a test's temporary directory
    pub syncs: bool,
    pub sync_idle_ms: u64,
    pub sync_interval_ms: u64,
    /// The map's size at open; by default twice the data file, and 64 MiB at least
    pub map_size_bytes: Option<usize>,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            schema: None,
            syncs: true,
            sync_idle_ms: 100,
            sync_interval_ms: 1000,
            map_size_bytes: None,
        }
    }
}

/// What opening did
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenReport {
    /// The schema's changes, each with what opening did about it; every addition when the database is new
    pub schema_changes: Vec<SchemaChange>,
}

/// An open database. Closing it, or dropping it, syncs what is owed and releases the environment
pub struct Database {
    path: PathBuf,
    read_only: bool,
    store: Mutex<Option<Arc<Store>>>,
    schema: Option<Schema>,
    /// The schema the engine holds writes to: the database's, or one declaring nothing
    engine_schema: Arc<Schema>,
    report: OpenReport,
}

/// The schema of a database that has none: nothing declared
fn empty_schema() -> Schema {
    Schema {
        language: SCHEMA_LANGUAGE.to_owned(),
        collation: "root".to_owned(),
        roles: Default::default(),
        relations: Default::default(),
        types: Default::default(),
    }
}

impl Database {
    /// Opens the database at `path`, creating the directory and its files unless `read_only`
    pub fn open(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self> {
        let store = Store::open(
            path.as_ref(),
            StoreOptions {
                read_only: options.read_only,
                syncs: options.syncs,
                sync_idle: Duration::from_millis(options.sync_idle_ms),
                sync_interval: Duration::from_millis(options.sync_interval_ms),
                map_size_bytes: options.map_size_bytes,
            },
        )?;
        let (schema, report) = match apply_schema(&store, options.schema) {
            Ok(applied) => applied,
            Err(error) => {
                let _ = store.close();
                return Err(error);
            }
        };
        Ok(Self {
            path: store.path().to_path_buf(),
            read_only: options.read_only,
            store: Mutex::new(Some(Arc::new(store))),
            engine_schema: Arc::new(schema.clone().unwrap_or_else(empty_schema)),
            schema,
            report,
        })
    }

    /// The database's directory
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn is_open(&self) -> bool {
        self.store.lock().expect("the store slot's lock is never poisoned").is_some()
    }

    /// The schema the database holds, if one was ever given
    pub fn schema(&self) -> Option<&Schema> {
        self.schema.as_ref()
    }

    /// What opening did
    pub fn open_report(&self) -> &OpenReport {
        &self.report
    }

    pub(crate) fn engine_schema(&self) -> Arc<Schema> {
        Arc::clone(&self.engine_schema)
    }

    pub(crate) fn store(&self) -> Result<Arc<Store>> {
        self.store
            .lock()
            .expect("the store slot's lock is never poisoned")
            .clone()
            .ok_or_else(|| Error::new(ErrorCode::Closed, format!("The database at {} is closed", self.path.display())))
    }

    /// Waits for every unit committed before the call to be on disk; resolved at once when nothing is owed, or when
    /// syncs are off or the database is read-only
    pub fn durable(&self) -> Result<Durable> {
        Ok(self.store()?.durable().unwrap_or_else(Durable::resolved))
    }

    /// Group commit's syncs so far
    pub fn syncs(&self) -> Result<u64> {
        Ok(self.store()?.syncs())
    }

    /// The map's size now, in bytes
    pub fn map_size(&self) -> Result<usize> {
        Ok(self.store()?.map_size())
    }

    /// Syncs what is owed and releases the environment; closing a closed database does nothing
    pub fn close(&self) -> Result<()> {
        let Some(mut store) = self.store.lock().expect("the store slot's lock is never poisoned").take() else {
            return Ok(());
        };
        // An operation that took the store before the close finishes first; none keeps it for long
        loop {
            match Arc::try_unwrap(store) {
                Ok(store) => return store.close(),
                Err(shared) => {
                    store = shared;
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Compares the given schema with the stored one, refuses a change opening can't make, and stores the given one
fn apply_schema(store: &Store, given: Option<Schema>) -> Result<(Option<Schema>, OpenReport)> {
    let stored = match store.meta(meta::SCHEMA)? {
        Some(bytes) => Some(Schema::from_canonical(&String::from_utf8_lossy(&bytes))?),
        None => None,
    };
    let Some(given) = given else { return Ok((stored, OpenReport::default())) };
    if stored.as_ref().is_some_and(|s| s.hash() == given.hash()) {
        return Ok((stored, OpenReport::default()));
    }
    let changes = diff(stored.as_ref().unwrap_or(&empty_schema()), &given);
    if store.is_read_only() {
        return Err(Error::new(
            ErrorCode::SchemaChangeRefused,
            format!("The database at {} is open read-only, so its schema can't change", store.path().display()),
        ));
    }
    let refused: Vec<String> = changes.iter().filter(|c| c.effect == Effect::Refused).map(|c| c.path.clone()).collect();
    if !refused.is_empty() {
        return Err(Error::new(
            ErrorCode::SchemaChangeRefused,
            format!("The schema changes what can't change once stored: {}", refused.join(", ")),
        ));
    }
    let canonical = given.canonical();
    let hash = given.hash();
    store.write(
        |txn, db| {
            put_meta(txn, db, meta::SCHEMA, canonical.as_bytes())?;
            put_meta(txn, db, meta::SCHEMA_HASH, hash.as_bytes())
        },
        true,
    )?;
    Ok((Some(given), OpenReport { schema_changes: changes }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn note_schema(extra_field: Option<&str>) -> Schema {
        let mut fields = json!({ "title": { "type": "string" } });
        if let Some(name) = extra_field {
            fields[name] = json!({ "type": "string", "optional": true });
        }
        Schema::from_json(&json!({
            "$schema": SCHEMA_LANGUAGE,
            "types": { "Note": { "shortCode": { "prefix": "NTE" }, "fields": fields } }
        }))
        .unwrap()
    }

    fn quiet() -> OpenOptions {
        OpenOptions { syncs: false, ..Default::default() }
    }

    #[test]
    fn opens_creates_and_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = Database::open(&path, OpenOptions::default()).unwrap();
        assert!(db.is_open());
        assert!(path.join("data.mdb").is_file());
        db.close().unwrap();
        assert!(!db.is_open());
        db.close().unwrap();
        assert_eq!(db.durable().err().unwrap().code, ErrorCode::Closed);
    }

    #[test]
    fn refuses_a_second_open_in_one_process_and_allows_one_after_close() {
        let dir = tempfile::tempdir().unwrap();
        let first = Database::open(dir.path(), quiet()).unwrap();
        assert_eq!(Database::open(dir.path(), quiet()).err().unwrap().code, ErrorCode::AlreadyOpen);
        first.close().unwrap();
        Database::open(dir.path(), quiet()).unwrap();
    }

    #[test]
    fn a_read_only_open_needs_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let error = Database::open(dir.path(), OpenOptions { read_only: true, ..quiet() }).err().unwrap();
        assert_eq!(error.code, ErrorCode::NoDatabase);
        drop(Database::open(dir.path(), quiet()).unwrap());
        let db = Database::open(dir.path(), OpenOptions { read_only: true, ..quiet() }).unwrap();
        assert!(db.is_read_only());
    }

    #[test]
    fn stores_the_schema_reports_its_changes_and_keeps_it_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let first = Database::open(dir.path(), OpenOptions { schema: Some(note_schema(None)), ..quiet() }).unwrap();
        let added: Vec<&str> = first.open_report().schema_changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(added, ["types.Note"]);
        first.close().unwrap();

        // Opened again without a schema, it holds the stored one
        let again = Database::open(dir.path(), quiet()).unwrap();
        assert_eq!(again.schema(), Some(&note_schema(None)));
        assert!(again.open_report().schema_changes.is_empty());
        again.close().unwrap();

        let changed =
            Database::open(dir.path(), OpenOptions { schema: Some(note_schema(Some("body"))), ..quiet() }).unwrap();
        let paths: Vec<&str> = changed.open_report().schema_changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["types.Note.fields.body"]);
        changed.close().unwrap();

        let read_only = Database::open(dir.path(), OpenOptions { read_only: true, ..quiet() }).unwrap();
        assert_eq!(read_only.schema(), Some(&note_schema(Some("body"))));
    }

    #[test]
    fn refuses_a_change_opening_cant_make_and_keeps_the_stored_schema() {
        let dir = tempfile::tempdir().unwrap();
        Database::open(dir.path(), OpenOptions { schema: Some(note_schema(None)), ..quiet() })
            .unwrap()
            .close()
            .unwrap();
        let mut recoded = note_schema(None);
        recoded.types.get_mut("Note").unwrap().short_code.as_mut().unwrap().prefix = "NOT".to_owned();
        let error = Database::open(dir.path(), OpenOptions { schema: Some(recoded), ..quiet() }).err().unwrap();
        assert_eq!(error.code, ErrorCode::SchemaChangeRefused);
        assert!(error.message.contains("types.Note.shortCode.prefix"), "{}", error.message);
        // The refused open released the files, and the stored schema is as it was
        let db = Database::open(dir.path(), quiet()).unwrap();
        assert_eq!(db.schema(), Some(&note_schema(None)));
        db.close().unwrap();

        let read_only = OpenOptions { read_only: true, schema: Some(note_schema(Some("body"))), ..quiet() };
        assert_eq!(Database::open(dir.path(), read_only).err().unwrap().code, ErrorCode::SchemaChangeRefused);
    }
}
