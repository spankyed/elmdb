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
    /// Every commit synced as it is made, so that one that returned survives the machine losing power, at the price of a
    /// sync per commit (`docs/architecture/engine.md`, Durability). It syncs whatever `syncs` says, group commit and its
    /// timings are then unused, and it is off for a read-only database, which is never written
    pub sync_each_commit: bool,
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
            sync_each_commit: false,
            sync_idle_ms: 100,
            sync_interval_ms: 1000,
            map_size_bytes: None,
        }
    }
}

/// What opening did
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct OpenReport {
    /// The schema's changes, each with what opening did about it; every addition when the database is new
    pub schema_changes: Vec<SchemaChange>,
    /// A restore this open or an earlier one made, which every open reports until a caller says it read it
    /// (`Database::acknowledge_restore`)
    pub restored: Option<Restore>,
    /// The record beside the data file named another file than this one, put where the database's was: the file was
    /// opened as it is
    pub not_borne_out: bool,
}

/// A power loss or a failed sync took the database back to its last synced commit: the commits after it are gone
/// (`docs/roadmap/synced-commit.md`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Restore {
    /// The sequence the database opened at: none where the open that restored it couldn't write it down, a read-only
    /// open on a filesystem it can't write
    pub sequence: Option<u64>,
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
    /// The index building, which outlives each thread that does it: its queue, what waits for it, its thread, and
    /// what a schema change and a close tell it
    pub(crate) building: crate::engine::Building,
    /// The tracked reads' read sets, and the database's watches
    pub(crate) tracks: crate::engine::watch::SharedTracks,
    /// Held by a rebuild from before it makes room in the map until its commit has been delivered: rebuilds asked on
    /// several threads at once run one after another, so the map grows for one of them and none redoes another's work
    /// beside it. A watch's listener the rebuild's commit runs may rebuild in turn, on the thread that holds it: so the
    /// lock lets that thread in again
    pub(crate) rebuilding: parking_lot::ReentrantMutex<()>,
}

/// The schema of a database that has none: nothing declared
fn empty_schema() -> Schema {
    Schema {
        language: SCHEMA_LANGUAGE.to_owned(),
        collation: "root".to_owned(),
        roles: Default::default(),
        relations: Default::default(),
        types: Default::default(),
        ranks: Default::default(),
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
    /// A schema as the database holds it, with the fingerprint stored beside it. A fingerprint computed again could
    /// differ, the canonical form having since gained a field, and would then refuse every read of an unchanged
    /// database as changed by another process
    pub(crate) fn stored(schema: Option<Schema>, hash: Option<Vec<u8>>) -> Self {
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
                sync_each_commit: options.sync_each_commit,
                sync_idle: Duration::from_millis(options.sync_idle_ms),
                sync_interval: Duration::from_millis(options.sync_interval_ms),
                map_size_bytes: options.map_size_bytes,
            },
        )?;
        let AppliedSchema { schema, hash, report, unbuilt, commit } = match apply_schema(&store, options.schema, None) {
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
        let engine_schema = Arc::new(EngineSchema::stored(schema.clone(), hash));
        let report = OpenReport { restored: store.restored(), not_borne_out: store.not_borne_out(), ..report };
        let store = Arc::new(store);
        let tracks: crate::engine::watch::SharedTracks = Default::default();
        let observing = Arc::new(RwLock::new(Arc::clone(&engine_schema)));
        let building = crate::engine::Building::default();
        if !options.read_only {
            building.queue(unbuilt);
            // Where the system refuses the thread, the work waits queued for the next write or wait to start it
            let _ = building.start(|| target(&store, &engine_schema, &tracks, &observing));
        }
        Ok(Self {
            path: store.path().to_path_buf(),
            read_only: options.read_only,
            store: Mutex::new(Some(store)),
            engine_schema: observing,
            schema: RwLock::new(schema),
            report,
            building,
            tracks,
            rebuilding: Default::default(),
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
        // Asked again under the lock, for a unit begun since: it holds the write transaction the change needs
        if store.unit_is_open() {
            return Err(Error::new(ErrorCode::UnitOpen, "A unit is open: commit or abort it, then change the schema"));
        }
        // The building is told that what it builds to is changing, and its thread is waited for by no one:
        // its work is for the schema held until now and goes, the piece its thread is on is told to stop, and a
        // commit of that piece after the change is refused by the fingerprint the change stores
        // (`engine::builder_commits`). The schema's lock is held from here to the builder's being told what the
        // change left, so no commit queues it work and no close takes it in between, and a wait for the builds
        // asked for meanwhile waits for that (`Core::wait`)
        let change = self.building.begin_change();
        run_point!(SCHEMA_CHANGE_BEGUN);
        let mut applied = apply_schema(&store, Some(schema), Some(&current));
        if let Ok(applied) = &applied {
            *current = Arc::new(EngineSchema::stored(applied.schema.clone(), applied.hash.clone()));
            *self.schema.write() = applied.schema.clone();
        }
        if applied.is_err() {
            run_point!(SCHEMA_RECOVERY);
        }
        let unbuilt = match &mut applied {
            Ok(applied) => Some(applied.unbuilt.clone()),
            Err(original) => match crate::engine::remaining_indexes(&store, &current) {
                Ok(unbuilt) => Some(unbuilt),
                Err(secondary) => {
                    original.message.push_str(&format!("; refreshing index state also failed: {secondary}"));
                    None
                }
            },
        };
        // Ended before the change's commit is delivered: a listener it tells may wait for the builds
        {
            // A read-only database builds nothing, whether its change was refused or changed nothing
            match unbuilt.filter(|_| !self.read_only) {
                Some(unbuilt) => {
                    let commits = builder_commits_of(&store, &current, &self.tracks, &self.engine_schema);
                    self.building.end_change(change, Arc::clone(&current), commits, unbuilt);
                }
                // The change wasn't made and what is left to build couldn't be read: the building has back the
                // work the change took, and what had failed stays failed
                None => drop(change),
            }
            // A thread for what the change left, where none runs
            if !self.read_only {
                // Where the system refuses the thread, the work waits queued for the next write or wait to start it
                let _ = self.building.start(|| target(&store, &current, &self.tracks, &self.engine_schema));
            }
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

    /// Stops the building's thread, leaving what is queued for another (`Building::pause`)
    #[cfg(test)]
    pub(crate) fn pause_builder(&self) {
        self.building.pause();
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

    /// Waits for every unit committed before the call to be on disk; resolved at once when nothing is owed, when
    /// syncs are off or the database is read-only, or when each commit is synced as it is made (`sync_each_commit`)
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
    /// Refused from a listener of this database, a watch's or the builder's commits' (`CALLBACK_ACTIVE`); close
    /// after it returns
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

    /// Tells `listener` of each commit the index builder makes that wrote into a tracked read, with its sequence and those
    /// reads' tokens, or stops telling anyone: a write answers its own, and a builder's commit has nobody to answer to.
    /// There is one listener: a second is refused (`ALREADY_LISTENING`), since it would leave the first unheard, and `None`
    /// or closing the database drops it. It runs on the builder's thread, so it hands off and returns
    pub fn hear_builder_commits(&self, listener: Option<crate::engine::watch::BuilderCommitListener>) -> Result<()> {
        self.tracks.lock().hear_builder_commits(listener)
    }

    /// Tells `listener` of commits other processes make, right after each, which `committed_elsewhere` then answers. There
    /// is one listener: a second is refused (`ALREADY_LISTENING`), and closing the database drops it. It runs on a thread
    /// of its own, so it hands off and returns. Index work those commits left owed is queued first, since that process may
    /// have exited with its builder part way. It files a mailbox in the database's directory, which has to be writable
    pub fn hear_commits_elsewhere(&self, listener: crate::store::wake::WakeListener) -> Result<()> {
        let store = self.store()?;
        let weak = Arc::downgrade(&store);
        let (schemas, tracks) = (self.observing_schema(), Arc::clone(&self.tracks));
        let building = self.building.clone();
        let read_only = self.read_only;
        store.wake.listen(Arc::new(move || {
            // Work over rows another process has replaced can't commit until the holder adopts that, which queues it
            let current =
                |store: &Arc<crate::store::Store>| store.replaced_elsewhere().is_ok_and(|replaced| replaced.is_none());
            if let Some(store) = weak.upgrade().filter(|store| !read_only && current(store)) {
                let schema = Arc::clone(&schemas.read());
                if let Ok(owed) = crate::engine::remaining_indexes(&store, &schema) {
                    let builders = Builders {
                        building: building.clone(),
                        store,
                        schemas: Arc::clone(&schemas),
                        tracks: Arc::clone(&tracks),
                    };
                    builders.queue(owed);
                }
            }
            listener();
        }))
    }

    /// Lets go of the completed read transactions kept for a burst of reads, so that the next read opens one whatever time
    /// has passed: what makes a benchmark's count not follow how long its setup took
    #[doc(hidden)]
    pub fn release_idle_reads(&self) -> Result<()> {
        self.store()?.drop_idle();
        Ok(())
    }

    fn close_as(&self, wait: Option<Duration>, at_exit: bool) -> Result<bool> {
        // The whole close's: an operation that keeps the store, and at exit the schema's lock, share it
        let deadline = wait.map(|wait| std::time::Instant::now() + wait);
        // A watch's listener, or the listener of the builder's commits, which runs on the builder's thread: a close
        // waits for the operation the listener is called from, and for the store that thread holds
        if crate::engine::watch::in_callback(&self.tracks) {
            return Err(Error::new(
                ErrorCode::CallbackActive,
                "A listener of this database is running on this thread: close the database after it returns",
            ));
        }
        // Refused before the schema's lock is waited for, which a check on another thread holds for its whole run: a
        // close beside an open unit answers at once. Asked again under the lock, for a unit begun meanwhile
        if self
            .store
            .lock()
            .expect("the store slot's lock is never poisoned")
            .as_ref()
            .is_some_and(|s| s.unit_is_open())
        {
            return Ok(false);
        }
        let (taken, mut closing) = {
            // Held while the builder is told to stop and the store is taken, so a schema change can't start a builder
            // after; released before either is waited for. At exit it is waited for until the deadline: on Windows the
            // exit handlers run after every other thread was ended, and one ended holding the schema for reading (a
            // write waiting for its sync) never lets it go. Given up on, the database is synced and left open
            let _schema = match (at_exit, deadline) {
                (true, Some(deadline)) => match self.engine_schema.try_write_until(deadline) {
                    Some(schema) => schema,
                    None => {
                        let store = self.store.lock().expect("the store slot's lock is never poisoned").clone();
                        // The caller's wait is spent: the record's locks are tried, and not waited for
                        if let Some(store) = store {
                            store.sync_owed(deadline)?;
                        }
                        return Ok(false);
                    }
                },
                _ => self.engine_schema.write(),
            };
            let mut slot = self.store.lock().expect("the store slot's lock is never poisoned");
            match slot.as_ref() {
                None => return Ok(true),
                Some(store) if store.unit_is_open() => return Ok(false),
                Some(_) => {}
            }
            // The building is told, which tells its thread to stop. Until this close says whether the database
            // closed, a wait for the builds waits for it to, and work a commit queues starts no thread, which would
            // hold the store this waits for
            let closing = self.building.begin_close();
            (slot.take(), closing)
        };
        // A build in progress stops between chunks, for the thread after it or the next open to finish. Its thread is
        // waited for with nothing held that it may wait for: it may be delivering a commit of its own to a watch,
        // whose read and whose listener's calls of the database take the schema's lock and the store's slot. At
        // exit it is let go of
        self.building.join(at_exit);
        if at_exit {
            // The thread was let go of unjoined, and may be running: whatever becomes of this close, a failed sync
            // or a panic among it, no thread is started beside that one, and the building is said to be over now
            closing.closed();
        }
        let Some(mut store) = taken else { return Ok(true) };
        run_point!(CLOSE_TAKEN);
        // An operation that took the store before the close finishes first, and none keeps it long. The slot is free
        // meanwhile, so an operation that asks for the store again is told the database is closed rather than waiting
        loop {
            match Arc::try_unwrap(store) {
                Ok(store) => {
                    // Dropped by the close that closes: one refused for an open unit, or given up on below, leaves the
                    // database open, and its listener is still to hear the builder's commits
                    self.tracks.lock().hear_builder_commits(None).expect("dropping the listener refuses nothing");
                    // At exit, within what is left of the caller's wait, which the loop has spent part of
                    let left = deadline.map_or(std::time::Duration::ZERO, |deadline| {
                        deadline.saturating_duration_since(std::time::Instant::now())
                    });
                    // The close can no longer give up, and whoever waits for it is told the database is closed.
                    // Said before the store closes, which waits for its own threads: a listener one of them runs
                    // may be among those waiting
                    closing.closed();
                    let closed = if at_exit { store.close_at_exit(left) } else { store.close() };
                    return closed.map(|()| true);
                }
                Err(shared) => {
                    if let Some(deadline) = deadline.filter(|deadline| std::time::Instant::now() >= *deadline) {
                        // The caller's wait is spent: the record's locks are tried, and not waited for
                        let synced = if at_exit { shared.sync_owed(deadline) } else { Ok(()) };
                        *self.store.lock().expect("the store slot's lock is never poisoned") = Some(shared);
                        synced?;
                        if at_exit {
                            // The process is exiting: nothing more is built here, which the building was told as
                            // its thread was let go of
                            return Ok(false);
                        }
                        run_point!(CLOSE_GAVE_UP);
                        // The database stays open: what is left to build is queued again, as an open queues it, or
                        // a read of an unbuilt index would be told to wait for no one. The close is said to be over
                        // first, which lets a thread be started for it. A close that gave up answers that it did:
                        // what keeps the builds from being queued, another process's schema or a close beside this
                        // one, is the next operation's to say
                        // Queued while the close is still said to be under way, so that a wait for the builds
                        // that this close's end wakes finds the work there, and not a moment in which nothing is
                        // queued and no thread runs. The thread is started once the close is over, which is what
                        // lets one be
                        if let Ok(store) = self.store() {
                            // Where it can't be read nothing is queued, and a wait for the builds reads it again
                            // itself, with no thread there, and says why it can't. The schema's read is let go
                            // at this statement's end, before the builders take it
                            let owed = crate::engine::remaining_indexes(&store, &self.engine_schema.read());
                            self.building.queue(owed.unwrap_or_default());
                        }
                        drop(closing);
                        if let Ok(Some(builders)) = self.builders() {
                            builders.queue(Vec::new());
                        }
                        return Ok(false);
                    }
                    store = shared;
                    // An operation that took the store before the close, a commit or another process's wake, may queue
                    // index work after it. It starts no thread, which would hold the store, while the close is under
                    // way (`Builders::queue`): the work waits in the queue for the close to give up, or goes with it
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}

impl Drop for Database {
    /// Closes the database, and stops its building for good whatever the close answered: one refused, for a unit
    /// open or from a listener, leaves its thread to end with nothing left to hold it to
    fn drop(&mut self) {
        let _ = self.close();
        self.building.stop();
    }
}

/// The installed declaration and pending builds, with a completed transaction's sync outcome kept separate.
struct AppliedSchema {
    schema: Option<Schema>,
    /// The fingerprint stored beside the schema, which the handle validates against
    hash: Option<Vec<u8>>,
    report: OpenReport,
    unbuilt: Vec<crate::engine::Work>,
    commit: Option<crate::store::Commit>,
}

/// How a builder's chunks commit under `schema`: as writes do
fn builder_commits_of(
    store: &Arc<Store>,
    schema: &Arc<EngineSchema>,
    tracks: &crate::engine::watch::SharedTracks,
    observing: &Arc<RwLock<Arc<EngineSchema>>>,
) -> crate::engine::BuilderCommits {
    crate::engine::builder_commits(Arc::clone(tracks), Arc::clone(store), Arc::clone(observing), Arc::clone(schema))
}

/// What a building's thread builds to: the store, `schema`, and how its chunks commit, as writes do
pub(crate) fn target(
    store: &Arc<Store>,
    schema: &Arc<EngineSchema>,
    tracks: &crate::engine::watch::SharedTracks,
    observing: &Arc<RwLock<Arc<EngineSchema>>>,
) -> crate::engine::Target {
    (Arc::clone(store), Arc::clone(schema), builder_commits_of(store, schema, tracks, observing))
}

/// The database's building and what starting its thread takes, which a unit holds to hand the building the pushes
/// its commit left
#[derive(Clone)]
pub(crate) struct Builders {
    pub(crate) building: crate::engine::Building,
    pub(crate) store: Arc<Store>,
    pub(crate) schemas: Arc<RwLock<Arc<EngineSchema>>>,
    pub(crate) tracks: crate::engine::watch::SharedTracks,
}

impl Builders {
    /// Queues work for the building, and starts its thread where none runs and there is work: a rank index a commit
    /// left with pushes waiting, or what an earlier thread left queued. None is started while a close is under way:
    /// it would hold the store the close waits for, and the work waits in the queue for a close that gives up
    pub(crate) fn queue(&self, work: Vec<crate::engine::Work>) {
        // A commit's work, whose commit has landed: a thread the system refuses leaves it queued, for the next write
        // or wait for the builds to start one, and the commit isn't answered with it
        let _ = self.start(work);
    }

    /// `queue`, answering where the system refused the thread (`THREAD_REFUSED`)
    pub(crate) fn start(&self, work: Vec<crate::engine::Work>) -> Result<()> {
        self.building.queue(work);
        // A commit that left nothing, and nothing waiting, takes no lock to say so
        if !self.building.has_work() {
            return Ok(());
        }
        // Held for reading, the schema keeps a close from beginning as a thread is started, and a schema change from
        // being made, which each hold it for writing throughout
        let schema = self.schemas.read();
        self.building.start(|| target(&self.store, &schema, &self.tracks, &self.schemas))
    }
}

impl Database {
    /// What a commit hands the pushes it left to; none in a read-only database, which never writes
    pub(crate) fn builders(&self) -> Result<Option<Builders>> {
        self.builders_of(self.store()?)
    }

    /// `builders`, for the store a commit is made in, taken before the commit: a close may take the store slot once the
    /// commit is made, and a commit that has landed must not be answered CLOSED for it
    pub(crate) fn builders_of(&self, store: Arc<Store>) -> Result<Option<Builders>> {
        if self.read_only {
            return Ok(None);
        }
        Ok(Some(Builders {
            building: self.building.clone(),
            store,
            schemas: self.observing_schema(),
            tracks: Arc::clone(&self.tracks),
        }))
    }
}

fn unchanged_schema(store: &Store, schema: Option<Schema>, hash: Option<Vec<u8>>) -> Result<AppliedSchema> {
    let (unbuilt, commit) =
        crate::engine::settle_indexes_committed(store, &EngineSchema::stored(schema.clone(), hash.clone()))?;
    Ok(AppliedSchema { schema, hash, report: OpenReport::default(), unbuilt, commit })
}

/// Compares declarations and atomically installs the schema, index removals and empty-index markers.
fn apply_schema(store: &Store, given: Option<Schema>, expected: Option<&EngineSchema>) -> Result<AppliedSchema> {
    // Validate the handle and read the declaration in one snapshot. Reading a fresh declaration first would let
    // a stale setter silently adopt or overwrite another process's schema.
    let bytes = store.read(|txn, db| {
        if let Some(expected) = expected {
            expected.validate(txn, db)?;
        }
        let schema = db.get(txn, &crate::store::meta_key(meta::SCHEMA))?.map(<[u8]>::to_vec);
        let hash = db.get(txn, &crate::store::meta_key(meta::SCHEMA_HASH))?.map(<[u8]>::to_vec);
        Ok((schema, hash))
    })?;
    let (bytes, stored_hash) = bytes;
    let stored = match bytes {
        Some(bytes) => Some(Schema::from_canonical(&String::from_utf8_lossy(&bytes))?),
        None => None,
    };
    // Whether the given schema means what the stored one does is asked of both in today's canonical form; which
    // fingerprint a handle validates against is the stored one, until a change writes another
    let Some(given) = given else { return unchanged_schema(store, stored, stored_hash) };
    if stored.as_ref().is_some_and(|s| s.hash() == given.hash()) {
        return unchanged_schema(store, stored, stored_hash);
    }
    let version = EngineSchema::stored(stored, stored_hash);
    if store.is_read_only() {
        return Err(Error::new(
            ErrorCode::SchemaChangeRefused,
            format!("The database at {} is open read-only, so its schema can't change", store.path().display()),
        ));
    }
    let canonical = given.canonical();
    let hash = given.hash();
    run_point!(SCHEMA_WRITE);
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
        hash: Some(hash.into_bytes()),
        report: OpenReport { schema_changes: changes, ..OpenReport::default() },
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

    #[test]
    fn an_open_after_a_loss_reports_the_restore_and_its_sequence_until_a_caller_acknowledges_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let note = |id: &str| -> crate::Batch {
            serde_json::from_value(
                json!({ "ops": [{ "op": "create", "type": "Note", "id": id, "fields": { "title": "t" } }] }),
            )
            .unwrap()
        };
        let options = OpenOptions {
            schema: Some(note_schema(None)),
            sync_idle_ms: 600_000,
            sync_interval_ms: 600_000,
            ..Default::default()
        };
        let db = Database::open(&path, options).unwrap();
        db.write(&note("Note-1"), true).unwrap();
        let synced = db.sequence().unwrap();
        db.write(&note("Note-2"), false).unwrap();
        // The files as a loss leaves them: the record written in another boot, a commit since it on the file
        let lost = |into: &Path| {
            std::fs::create_dir_all(into).unwrap();
            std::fs::copy(path.join("data.mdb"), into.join("data.mdb")).unwrap();
            let record = std::fs::read(path.join(crate::store::RECORD_FILE)).unwrap();
            std::fs::write(into.join(crate::store::RECORD_FILE), crate::store::record_of_another_boot(&record))
                .unwrap();
        };
        let restored = dir.path().join("restored");
        lost(&restored);
        db.close().unwrap();
        let opened = Database::open(&restored, OpenOptions::default()).unwrap();
        let report = opened.open_report();
        assert_eq!((report.restored, report.not_borne_out), (Some(Restore { sequence: Some(synced) }), false));
        assert_eq!(opened.sequence().unwrap(), synced, "opened at the record's commit");
        opened.acknowledge_restore().unwrap();
        opened.close().unwrap();
        let again = Database::open(&restored, OpenOptions::default()).unwrap();
        assert_eq!(again.open_report().restored, None, "reported again once acknowledged");
        again.close().unwrap();
        // The record beside another database's data file: opened as it is, and said so
        let other = tempfile::tempdir().unwrap();
        let elsewhere =
            Database::open(other.path(), OpenOptions { schema: Some(note_schema(None)), ..quiet() }).unwrap();
        elsewhere.write(&note("Note-9"), false).unwrap();
        elsewhere.close().unwrap();
        let swapped = dir.path().join("swapped");
        lost(&swapped);
        std::fs::copy(other.path().join("data.mdb"), swapped.join("data.mdb")).unwrap();
        let opened = Database::open(&swapped, quiet()).unwrap();
        assert_eq!((opened.open_report().restored, opened.open_report().not_borne_out), (None, true));
        opened.close().unwrap();
    }

    fn quiet() -> OpenOptions {
        OpenOptions { syncs: false, ..Default::default() }
    }

    // Each gives a margin past its wait for the flush of the data the exit makes, a full flush of the drive on Apple,
    // slow on a loaded machine
    #[test]
    fn an_exit_given_up_on_for_an_operation_s_store_tries_the_record_and_is_over_within_its_wait() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("db"), OpenOptions::default()).unwrap();
        // An operation holds the store past the wait, and a sync of another thread the record
        let store = db.store().unwrap();
        let held = store.hold_record();
        let began = std::time::Instant::now();
        assert_eq!(db.close_at_exit(Duration::from_millis(300)).unwrap_err().code, ErrorCode::Io);
        let took = began.elapsed();
        // Bounded, the record being held a minute: what a flush beside a loaded suite takes is no part of the test
        assert!(took < Duration::from_secs(10), "{took:?}, for a wait of 300 ms");
        // Its sync refused, the close still left nothing building: a thread may have been let go of unjoined
        assert!(db.building.closing(), "a thread could be started beside one a close at exit let go of");
        drop(held);
        drop(store);
    }

    #[test]
    fn an_exit_whose_store_comes_free_late_closes_within_what_is_left_of_its_wait() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("db"), OpenOptions::default()).unwrap();
        // An operation lets the store go 5 s into a wait of 6, and a sync of another thread holds the record past
        // both: the close ends at 6 s, and one given the whole wait again would take 11 s. A sleep and a wait end
        // late and never early, so the wrong close is never under 11 s, and the bound leaves the right one 4 s for
        // a loaded runner: it was 1.5 s on times half these, and then 2.5 s
        let store = db.store().unwrap();
        let held = store.hold_record();
        let began = std::time::Instant::now();
        let operation = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            drop(store);
        });
        assert_eq!(db.close_at_exit(Duration::from_secs(6)).unwrap_err().code, ErrorCode::Io);
        let took = began.elapsed();
        assert!(took < Duration::from_secs(10), "{took:?}, for a wait of 6 s");
        operation.join().unwrap();
        drop(held);
    }

    #[test]
    fn a_close_at_exit_gives_up_on_the_schema_a_thread_holds_past_its_wait() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(dir.path().join("db"), quiet()).unwrap());
        // The schema held for reading, as a write waiting for its sync holds it, by a thread the exit never sees end
        let held = db.engine_schema.read();
        let (closed, has_closed) = std::sync::mpsc::channel();
        let closing = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || closed.send(db.close_at_exit(Duration::from_millis(100))).unwrap())
        };
        let answer =
            has_closed.recv_timeout(Duration::from_secs(10)).expect("the close at exit gave up within its wait");
        assert!(!answer.unwrap(), "left open, the schema being held");
        drop(held);
        closing.join().unwrap();
        assert!(db.is_open());
        db.close().unwrap();
        assert!(!db.is_open());
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
    fn a_close_beside_an_open_unit_is_refused_without_waiting_for_a_check_on_another_thread() {
        let dir = tempfile::tempdir().unwrap();
        let db =
            Arc::new(Database::open(dir.path(), OpenOptions { schema: Some(note_schema(None)), ..quiet() }).unwrap());
        let unit = db.begin().unwrap();
        // A check holds the schema's lock for reading for its whole run, as this thread holds it
        let (held, holding) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let checker = {
            let schema = Arc::clone(&db.engine_schema);
            std::thread::spawn(move || {
                let _reading = schema.read();
                held.send(()).unwrap();
                let _ = released.recv_timeout(Duration::from_secs(10));
            })
        };
        holding.recv().unwrap();
        let (answered, answer) = std::sync::mpsc::channel();
        let closer = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || {
                let _ = answered.send(db.close().map_err(|error| error.code));
            })
        };
        let answer = answer.recv_timeout(Duration::from_secs(10));
        release.send(()).unwrap();
        checker.join().unwrap();
        closer.join().unwrap();
        assert_eq!(answer.expect("the close waited for the check before it was refused"), Err(ErrorCode::UnitOpen));
        drop(unit);
        db.close().unwrap();
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
