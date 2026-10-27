//! A database: one directory holding one LMDB environment, opened at most once per process, with its schema
//! (`docs/architecture/engine.md`, Opening).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use parking_lot::{RwLock, RwLockReadGuard};
use std::time::Duration;

use crate::error::{Error, ErrorCode, Result};
use crate::format::meta;
use crate::schema::{Effect, SCHEMA_LANGUAGE, Schema, SchemaChange, diff};
use crate::store::sync::Durable;
use crate::store::{Store, StoreOptions, put_meta};

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_SCHEMA_RECOVERY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static BEFORE_SCHEMA_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// How a database is opened
#[derive(Debug, Clone)]
pub struct OpenOptions {
    /// Open existing files without writing: every write is refused, and the stored schema is used
    pub read_only: bool,
    /// The schema to hold the database to. Opening with one that differs from the stored schema changes it
    /// (`docs/architecture/schema.md`, Changing a schema); `None` keeps the stored one
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
    schema: RwLock<Option<Schema>>,
    /// The schema the engine holds writes to: the database's, or one declaring nothing. A write holds it read-locked
    /// from before its transaction until it commits, and `set_schema` write-locked, so no write commits under a
    /// schema that changed while it ran
    engine_schema: Arc<RwLock<Arc<EngineSchema>>>,
    report: OpenReport,
    /// Builds the indexes that wait for it; none in a read-only database
    builder: Mutex<Option<crate::engine::Builder>>,
    /// The tracked reads' read sets, and the database's watches
    pub(crate) tracks: crate::engine::watch::SharedTracks,
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

/// A declaration and its cached SHA-256 fingerprint. Validate inside each transaction before using cached rules;
/// another process may have changed declarations after this handle opened. No schema serialization occurs per read.
pub(crate) struct EngineSchema {
    schema: Schema,
    hash: Option<Vec<u8>>,
    hash_key: Vec<u8>,
}

impl EngineSchema {
    pub(crate) fn new(schema: Option<Schema>) -> Self {
        let hash = schema.as_ref().map(|schema| schema.hash().into_bytes());
        Self { schema: schema.unwrap_or_else(empty_schema), hash, hash_key: crate::store::meta_key(meta::SCHEMA_HASH) }
    }

    pub(crate) fn validate(&self, txn: &heed::RoTxn<'_, heed::AnyTls>, db: crate::store::Db) -> Result<()> {
        if db.get(txn, &self.hash_key)? != self.hash.as_deref() {
            return Err(Error::new(
                ErrorCode::SchemaChanged,
                "Another process changed the database schema; close and reopen this handle before reading or writing",
            ));
        }
        Ok(())
    }
}

impl std::ops::Deref for EngineSchema {
    type Target = Schema;
    fn deref(&self) -> &Schema {
        &self.schema
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
        let AppliedSchema { schema, report, unbuilt, commit } = match apply_schema(&store, options.schema, None) {
            Ok(applied) => applied,
            Err(error) => {
                let _ = store.close();
                return Err(error);
            }
        };
        if let Some(commit) = commit
            && let Err(error) = commit.finish()
        {
            let _ = store.close();
            return Err(error);
        }
        let engine_schema = Arc::new(EngineSchema::new(schema.clone()));
        let store = Arc::new(store);
        let builder = (!options.read_only && !unbuilt.is_empty()).then(|| {
            let builder = crate::engine::Builder::start(Arc::clone(&store), Arc::clone(&engine_schema));
            builder.queue(unbuilt);
            builder
        });
        Ok(Self {
            path: store.path().to_path_buf(),
            read_only: options.read_only,
            store: Mutex::new(Some(store)),
            engine_schema: Arc::new(RwLock::new(engine_schema)),
            schema: RwLock::new(schema),
            report,
            builder: Mutex::new(builder),
            tracks: Default::default(),
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
    pub fn schema(&self) -> Option<Schema> {
        self.schema.read().clone()
    }

    /// Changes the schema of the open database as opening with `schema` would (`docs/architecture/schema.md`, Changing a schema),
    /// answering the changes and what was done about each. Refused while a unit is open (`UNIT_OPEN`), in a
    /// read-only database, and for a change opening refuses, which leaves the schema as it was
    pub fn set_schema(&self, schema: Schema) -> Result<Vec<SchemaChange>> {
        let store = self.store()?;
        if store.unit_is_open() {
            return Err(Error::new(ErrorCode::UnitOpen, "A unit is open: commit or abort it, then change the schema"));
        }
        let mut current = self.engine_schema.write();
        // Refused before the builder stops: a unit holding the write transaction would keep a build from finishing
        if store.unit_is_open() {
            return Err(Error::new(ErrorCode::UnitOpen, "A unit is open: commit or abort it, then change the schema"));
        }
        // The builder works to the schema it was started with, so it stops first and starts again on the one held
        // after, whichever that is
        if let Some(mut builder) = self.builder().take() {
            builder.stop();
        }
        let mut applied = apply_schema(&store, Some(schema), Some(&current));
        if let Ok(applied) = &applied {
            *current = Arc::new(EngineSchema::new(applied.schema.clone()));
            *self.schema.write() = applied.schema.clone();
        }
        #[cfg(test)]
        if applied.is_err() {
            BEFORE_SCHEMA_RECOVERY.with_borrow_mut(|hook| {
                if let Some(hook) = hook.take() {
                    hook();
                }
            });
        }
        let unbuilt = match &mut applied {
            Ok(applied) => applied.unbuilt.clone(),
            Err(original) => match crate::engine::remaining_indexes(&store, &current) {
                Ok(unbuilt) => unbuilt,
                Err(secondary) => {
                    original.message.push_str(&format!("; refreshing index state also failed: {secondary}"));
                    Vec::new()
                }
            },
        };
        if !unbuilt.is_empty() {
            let builder = crate::engine::Builder::start(Arc::clone(&store), Arc::clone(&current));
            builder.queue(unbuilt);
            *self.builder() = Some(builder);
        }
        drop(current);
        let AppliedSchema { report, commit, .. } = applied?;
        if let Some(commit) = commit {
            self.finish_admin_commit(&store, commit)?;
        }
        Ok(report.schema_changes)
    }

    /// What opening did
    pub fn open_report(&self) -> &OpenReport {
        &self.report
    }

    pub(crate) fn builder(&self) -> std::sync::MutexGuard<'_, Option<crate::engine::Builder>> {
        self.builder.lock().expect("the builder slot's lock is never poisoned")
    }

    #[cfg(test)]
    pub(crate) fn engine_schema(&self) -> Arc<EngineSchema> {
        Arc::clone(&self.engine_schema.read())
    }

    /// Shared schema source for watches delivered after a unit ends, including later callback-driven changes.
    pub(crate) fn observing_schema(&self) -> Arc<RwLock<Arc<EngineSchema>>> {
        Arc::clone(&self.engine_schema)
    }

    /// The engine's schema, held read-locked while an operation takes and uses its transaction
    pub(crate) fn schema_guard(&self) -> RwLockReadGuard<'_, Arc<EngineSchema>> {
        self.engine_schema.read()
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

    /// Syncs what is owed and releases the environment; closing a closed database does nothing. Refused while a unit
    /// is open (`UNIT_OPEN`): the unit holds the environment's write transaction, so commit or abort it first.
    /// Refused from this database's watch callback (`CALLBACK_ACTIVE`); close after the callback returns
    pub fn close(&self) -> Result<()> {
        if self.close_within(None)? {
            return Ok(());
        }
        Err(Error::new(
            ErrorCode::UnitOpen,
            "A unit is open on this database: commit or abort it, then close the database",
        ))
    }

    /// `close`, answering false rather than closing while a unit is open, and giving up after `wait` when an operation
    /// still holds the store. A database left open keeps its store, and closes later
    pub fn close_within(&self, wait: Option<Duration>) -> Result<bool> {
        self.close_as(wait, false)
    }

    /// `close_within` in a process that is exiting, which waits on no thread but the caller's: on Windows the exit
    /// handlers run after every other thread was ended, so the index builder is left unjoined and group commit's sync
    /// runs on this thread (`Store::close_at_exit`). A store an operation still holds after `wait` is synced and left
    /// open
    pub fn close_at_exit(&self, wait: Duration) -> Result<bool> {
        self.close_as(Some(wait), true)
    }

    fn close_as(&self, wait: Option<Duration>, at_exit: bool) -> Result<bool> {
        if crate::engine::watch::in_callback(&self.tracks) {
            return Err(Error::new(
                ErrorCode::CallbackActive,
                "A watch listener is running on this thread: close the database after the listener returns",
            ));
        }
        let taken = {
            // Held while the builder stops and the store is taken, so a schema change can't start a builder after;
            // released before the wait, which an operation finishing may need it for
            let _schema = self.engine_schema.write();
            let mut slot = self.store.lock().expect("the store slot's lock is never poisoned");
            match slot.as_ref() {
                None => return Ok(true),
                Some(store) if store.unit_is_open() => return Ok(false),
                Some(_) => {}
            }
            // A build in progress stops between chunks, for the next open to finish
            if let Some(mut builder) = self.builder().take() {
                if at_exit {
                    builder.abandon();
                } else {
                    builder.stop();
                }
            }
            slot.take()
        };
        let Some(mut store) = taken else { return Ok(true) };
        // An operation that took the store before the close finishes first, and none keeps it long. The slot is free
        // meanwhile, so an operation that asks for the store again is told the database is closed rather than waiting
        let deadline = wait.map(|wait| std::time::Instant::now() + wait);
        loop {
            match Arc::try_unwrap(store) {
                Ok(store) if at_exit => return store.close_at_exit(wait.unwrap_or_default()).map(|()| true),
                Ok(store) => return store.close().map(|()| true),
                Err(shared) => {
                    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                        let synced = if at_exit { shared.sync_owed() } else { Ok(()) };
                        *self.store.lock().expect("the store slot's lock is never poisoned") = Some(shared);
                        return synced.map(|()| false);
                    }
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

/// The installed declaration and pending builds, with a completed transaction's sync outcome kept separate.
struct AppliedSchema {
    schema: Option<Schema>,
    report: OpenReport,
    unbuilt: Vec<(String, String)>,
    commit: Option<crate::store::Commit>,
}

fn unchanged_schema(store: &Store, schema: Option<Schema>) -> Result<AppliedSchema> {
    let (unbuilt, commit) = crate::engine::settle_indexes_committed(store, &EngineSchema::new(schema.clone()))?;
    Ok(AppliedSchema { schema, report: OpenReport::default(), unbuilt, commit })
}

/// Compares declarations and atomically installs the schema, index removals and empty-index markers.
fn apply_schema(store: &Store, given: Option<Schema>, expected: Option<&EngineSchema>) -> Result<AppliedSchema> {
    // Validate the handle and read the declaration in one snapshot. Reading a fresh declaration first would let
    // a stale setter silently adopt or overwrite another process's schema.
    let bytes = store.read(|txn, db| {
        if let Some(expected) = expected {
            expected.validate(txn, db)?;
        }
        Ok(db.get(txn, &crate::store::meta_key(meta::SCHEMA))?.map(<[u8]>::to_vec))
    })?;
    let stored = match bytes {
        Some(bytes) => Some(Schema::from_canonical(&String::from_utf8_lossy(&bytes))?),
        None => None,
    };
    let Some(given) = given else { return unchanged_schema(store, stored) };
    if stored.as_ref().is_some_and(|s| s.hash() == given.hash()) {
        return unchanged_schema(store, stored);
    }
    let version = EngineSchema::new(stored);
    if store.is_read_only() {
        return Err(Error::new(
            ErrorCode::SchemaChangeRefused,
            format!("The database at {} is open read-only, so its schema can't change", store.path().display()),
        ));
    }
    let canonical = given.canonical();
    let hash = given.hash();
    #[cfg(test)]
    BEFORE_SCHEMA_WRITE.with_borrow_mut(|hook| {
        if let Some(hook) = hook.take() {
            hook();
        }
    });
    let ((unbuilt, changes), commit) = store.write_committed(
        |txn, db| {
            let held = crate::engine::held_in(txn, db, &version, &given)?;
            let changes = diff(&version, &given, &held);
            let refused: Vec<String> =
                changes.iter().filter(|c| c.effect == Effect::Refused).map(|c| c.path.clone()).collect();
            if !refused.is_empty() {
                return Err(Error::new(
                    ErrorCode::SchemaChangeRefused,
                    format!("The schema changes what can't change once stored: {}", refused.join(", ")),
                ));
            }
            crate::engine::drop_index_data(db, txn, &changes)?;
            put_meta(txn, db.main, meta::SCHEMA, canonical.as_bytes())?;
            put_meta(txn, db.main, meta::SCHEMA_HASH, hash.as_bytes())?;
            let mut state = crate::engine::unit::UnitState::default();
            let unbuilt = crate::engine::settle_indexes_in(&mut crate::engine::unit::Txn {
                txn,
                db,
                state: &mut state,
                schema: &given,
                now: 0.0,
            })?;
            Ok((unbuilt, changes))
        },
        true,
    )?;
    Ok(AppliedSchema {
        schema: Some(given),
        report: OpenReport { schema_changes: changes },
        unbuilt,
        commit: Some(commit),
    })
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
        assert_eq!(again.schema(), Some(note_schema(None)));
        assert!(again.open_report().schema_changes.is_empty());
        again.close().unwrap();

        let changed =
            Database::open(dir.path(), OpenOptions { schema: Some(note_schema(Some("body"))), ..quiet() }).unwrap();
        let paths: Vec<&str> = changed.open_report().schema_changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["types.Note.fields.body"]);
        changed.close().unwrap();

        let read_only = Database::open(dir.path(), OpenOptions { read_only: true, ..quiet() }).unwrap();
        assert_eq!(read_only.schema(), Some(note_schema(Some("body"))));
    }

    #[test]
    fn changes_the_schema_of_an_open_database_and_refuses_what_opening_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path(), OpenOptions { schema: Some(note_schema(None)), ..quiet() }).unwrap();
        let changes = db.set_schema(note_schema(Some("body"))).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["types.Note.fields.body"]);
        assert_eq!(db.schema(), Some(note_schema(Some("body"))));
        assert!(db.set_schema(note_schema(Some("body"))).unwrap().is_empty(), "the same schema changes nothing");

        let mut recoded = note_schema(Some("body"));
        recoded.types.get_mut("Note").unwrap().short_code.as_mut().unwrap().prefix = "NOT".to_owned();
        assert_eq!(db.set_schema(recoded).err().unwrap().code, ErrorCode::SchemaChangeRefused);
        assert_eq!(db.schema(), Some(note_schema(Some("body"))));

        // An open unit holds the one write transaction, which the change needs
        let unit = db.begin().unwrap();
        assert_eq!(db.set_schema(note_schema(None)).err().unwrap().code, ErrorCode::UnitOpen);
        drop(unit);
        db.set_schema(note_schema(None)).unwrap();
        db.close().unwrap();
        let again = Database::open(dir.path(), quiet()).unwrap();
        assert_eq!(again.schema(), Some(note_schema(None)), "the change is stored");
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
        assert_eq!(db.schema(), Some(note_schema(None)));
        db.close().unwrap();

        let read_only = OpenOptions { read_only: true, schema: Some(note_schema(Some("body"))), ..quiet() };
        assert_eq!(Database::open(dir.path(), read_only).err().unwrap().code, ErrorCode::SchemaChangeRefused);
    }
}
