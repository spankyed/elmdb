//! What commits tell tracked reads (`docs/architecture/engine.md`, Change sets and watch): the read sets filed under a
//! caller's tokens and under the database's own watches, and the watches, each a set of reads run again after every
//! commit that wrote into what it read last, its listener handed each answer that differs from the one before.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::database::EngineSchema;
use crate::error::{Error, ErrorCode, Result};
use crate::store::Store;

use super::plan::Reads;
use super::tracking::{ReadSet, Recording, TrackIndex};
use super::unit::Snapshot;

thread_local! {
    // A destructor-free TLS cell remains usable while other TLS destructors deliver watches. Nodes live on the
    // synchronous delivery stack, so this needs neither heap allocation nor a destructor to free per-thread state.
    static CALLBACKS: std::cell::Cell<*const CallbackScope> = const { std::cell::Cell::new(std::ptr::null()) };
}

/// A pinned node in this thread's stack of nested deliveries.
struct CallbackScope {
    tracks: usize,
    previous: *const Self,
}

impl CallbackScope {
    fn run<R>(tracks: &SharedTracks, run: impl FnOnce() -> R) -> R {
        let scope = std::pin::pin!(Self { tracks: Arc::as_ptr(tracks) as usize, previous: CALLBACKS.get() });
        CALLBACKS.set(&*scope);
        run()
    }
}

impl Drop for CallbackScope {
    fn drop(&mut self) {
        CALLBACKS.set(self.previous);
    }
}

pub(crate) fn in_callback(tracks: &SharedTracks) -> bool {
    let identity = Arc::as_ptr(tracks) as usize;
    let mut node = CALLBACKS.get();
    while !node.is_null() {
        // SAFETY: every published node is pinned on this thread's synchronous CallbackScope::run stack. Its Drop
        // restores the previous pointer before its storage expires, including during unwinding. No pointer crosses
        // threads and each tracks identity remains valid while delivery borrows its SharedTracks.
        let scope = unsafe { &*node };
        if scope.tracks == identity {
            return true;
        }
        node = scope.previous;
    }
    false
}

/// Who filed a read set: a caller, by the token its reads named, or one of the database's watches
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Owner {
    Token(u32),
    Watch(u64),
}

/// What a watch hands its listener: the reads' answer, as `read` gives it, or what they failed with
pub type Listener = Box<dyn FnMut(&Result<Vec<u8>>) + Send>;

struct Watching {
    reads: Reads,
    /// The last answer delivered
    last: Option<Vec<u8>>,
    /// Out while its listener runs
    listener: Option<Listener>,
    /// A commit named it while it was being delivered, so it runs again before it is put back
    again: bool,
}

/// The read sets filed, and the watches
#[derive(Default)]
pub struct Tracks {
    index: TrackIndex<Owner>,
    watches: BTreeMap<u64, Watching>,
    next: u64,
    pending_reads: BTreeMap<u64, u32>,
    next_read: u64,
}

pub type SharedTracks = Arc<Mutex<Tracks>>;

/// What a commit touched: the callers' tokens, and the watches to run again
#[derive(Debug, Default)]
pub struct Touched {
    pub tokens: Vec<u32>,
    pub watches: Vec<u64>,
}

impl Tracks {
    /// Whether anything is filed: a unit that begins when nothing is logs no keys
    pub fn is_empty(&self) -> bool {
        self.index.is_empty() && self.pending_reads.is_empty()
    }

    pub fn file(&mut self, token: u32, reads: ReadSet) {
        self.index.add(&Owner::Token(token), reads);
    }

    pub fn untrack(&mut self, token: u32) {
        self.index.remove(&Owner::Token(token));
        self.pending_reads.retain(|_, pending| *pending != token);
    }

    /// The tokens and ranges filed for callers and watches: `(owners, keys, ranges)`
    pub fn stats(&self) -> (usize, usize, usize) {
        let (keys, ranges) = self.index.size();
        (self.index.owners().len(), keys, ranges)
    }

    /// Who a commit that wrote `written` touched; every owner when it isn't known what it wrote
    pub fn touched(&self, written: Option<&HashSet<Vec<u8>>>) -> Touched {
        let owners: BTreeSet<Owner> = match written {
            Some(keys) => self.index.touched_by(keys.iter().map(Vec::as_slice)),
            None => self.index.owners(),
        };
        let mut touched = Touched::default();
        for owner in owners {
            match owner {
                Owner::Token(token) => touched.tokens.push(token),
                Owner::Watch(id) => touched.watches.push(id),
            }
        }
        touched
    }

    /// Every watch, to run again after a commit whose writes aren't known
    pub fn all_watches(&self) -> Vec<u64> {
        self.watches.keys().copied().collect()
    }
}

/// Runs `reads` on a snapshot of the last commit, answering them with what they read. A snapshot that could not be
/// taken read nothing, so the whole store stands for what they read: any commit then names them, where an empty read
/// set would leave whoever filed it on the failure for good
pub(crate) fn run_tracked(store: &Store, schema: &EngineSchema, reads: &Reads) -> (Result<Vec<u8>>, ReadSet) {
    run_point!(WATCH_READ);
    let ran = store.read(|txn, db| {
        schema.validate(txn, db)?;
        let snapshot = Snapshot { txn, db, schema };
        let recording = Recording::new(&snapshot);
        let answer = super::reads::run(&recording, &reads.reads);
        Ok((answer, recording.into_reads()))
    });
    ran.unwrap_or_else(|error| (Err(error), ReadSet { keys: BTreeSet::new(), ranges: vec![(Vec::new(), None)] }))
}

/// Registers a watch of `reads` and delivers its first answer
pub(crate) fn watch(
    tracks: &SharedTracks,
    store: &Store,
    schema: &RwLock<Arc<EngineSchema>>,
    reads: Reads,
    listener: Listener,
) -> Result<u64> {
    let id = {
        let mut held = tracks.lock();
        let id = held.next;
        held.next += 1;
        held.watches.insert(id, Watching { reads, last: None, listener: Some(listener), again: false });
        id
    };
    if let Some((_, code)) = deliver(tracks, store, schema, &[id]).into_iter().find(|(removed, _)| *removed == id) {
        return Err(Error::new(code, "The initial watch execution panicked; the watch was removed"));
    }
    Ok(id)
}

/// Ends a watch: nothing is delivered to it after
pub fn unwatch(tracks: &SharedTracks, id: u64) {
    let mut held = tracks.lock();
    held.watches.remove(&id);
    held.index.remove(&Owner::Watch(id));
}

/// Runs each watch again and hands its listener the answer when it differs from the last. Sequence validation and
/// dependency registration share the commit-notification lock, so an intervening commit causes a retry. The listener
/// runs outside it, so it may read, write or end a watch itself. A watch a commit names while
/// its listener runs is run again once the listener returns. A read that fails is handed to the listener as such.
/// An unwinding listener is removed and its id returned; other listeners still run
pub(crate) fn deliver(
    tracks: &SharedTracks,
    store: &Store,
    schema: &RwLock<Arc<EngineSchema>>,
    ids: &[u64],
) -> Vec<(u64, ErrorCode)> {
    let mut panicked = Vec::new();
    for &id in ids {
        // The scope also covers listener destruction: its captures may own the last database reference while
        // a committing unit still holds the store. Database::drop must not wait on that same stack frame.
        CallbackScope::run(tracks, || {
            let mut listener = {
                let mut held = tracks.lock();
                let Some(watching) = held.watches.get_mut(&id) else { return };
                match watching.listener.take() {
                    Some(listener) => Some(listener),
                    None => {
                        watching.again = true;
                        return;
                    }
                }
            };
            let mut calling_listener = false;
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                while let Some((answer, changed)) = read_watch(tracks, store, schema, id) {
                    if changed {
                        calling_listener = true;
                        listener.as_mut().expect("listener remains local until restored")(&answer);
                        calling_listener = false;
                    }
                    let mut held = tracks.lock();
                    let Some(watching) = held.watches.get_mut(&id) else {
                        held.index.remove(&Owner::Watch(id));
                        break;
                    };
                    if !watching.again {
                        watching.listener = listener.take();
                        break;
                    }
                }
            }));
            let dropped = catch_unwind(AssertUnwindSafe(|| drop(listener.take())));
            if outcome.is_err() || dropped.is_err() {
                unwatch(tracks, id);
                panicked.push((
                    id,
                    if calling_listener || dropped.is_err() {
                        ErrorCode::CallbackPanicked
                    } else {
                        ErrorCode::WatchPanicked
                    },
                ));
            }
        });
    }
    panicked
}

/// Uses the same bounded sequence validation as token reads, preserving any notification delivered while the
/// query runs. Removal during execution cancels delivery and registration.
fn read_watch(
    tracks: &SharedTracks,
    store: &Store,
    schema: &RwLock<Arc<EngineSchema>>,
    id: u64,
) -> Option<(Result<Vec<u8>>, bool)> {
    // Acquire schema before tracking: writes take the same order. Drop this guard before any listener runs.
    let held = schema.read();
    read_watch_with(tracks, store, id, |reads| run_tracked(store, &held, reads))
}

fn read_watch_with(
    tracks: &SharedTracks,
    store: &Store,
    id: u64,
    mut run: impl FnMut(&Reads) -> (Result<Vec<u8>>, ReadSet),
) -> Option<(Result<Vec<u8>>, bool)> {
    for attempt in 0..3 {
        let mut held = tracks.lock();
        let watching = held.watches.get_mut(&id)?;
        watching.again = false;
        let reads = watching.reads.clone();
        let (answer, read) = if attempt < 2 {
            drop(held);
            let before = store.sequence();
            let ran = run(&reads);
            held = tracks.lock();
            if before != store.sequence() {
                continue;
            }
            ran
        } else {
            run(&reads)
        };
        held.watches.get(&id)?;
        held.index.remove(&Owner::Watch(id));
        held.index.add(&Owner::Watch(id), read);
        let watching = held.watches.get_mut(&id).expect("held under the lock");
        let changed = match &answer {
            Ok(bytes) => watching.last.as_ref() != Some(bytes),
            Err(_) => true,
        };
        watching.last = answer.as_ref().ok().cloned();
        return Some((answer, changed));
    }
    unreachable!("the last attempt runs under the tracking lock")
}

/// An in-flight token read can be canceled by untrack. Drop also cleans up if read execution unwinds.
struct PendingRead {
    tracks: SharedTracks,
    id: u64,
}

impl Drop for PendingRead {
    fn drop(&mut self) {
        self.tracks.lock().pending_reads.remove(&self.id);
    }
}

/// Executes outside the tracking lock when no commit intervenes. Validation and dependency registration share the
/// commit-notification lock: a later commit names the filed read. One retry avoids unnecessary serialization; a
/// second conflict falls back to the locked path, bounding retries even under continuous writes.
pub(crate) fn read_tracked(
    tracks: &SharedTracks,
    store: &Store,
    schema: &EngineSchema,
    reads: &Reads,
    token: u32,
) -> Result<Vec<u8>> {
    read_registered(tracks, store, token, || run_tracked(store, schema, reads))
}

fn read_registered(
    tracks: &SharedTracks,
    store: &Store,
    token: u32,
    mut run: impl FnMut() -> (Result<Vec<u8>>, ReadSet),
) -> Result<Vec<u8>> {
    let pending = {
        let mut held = tracks.lock();
        let id = held.next_read;
        held.next_read += 1;
        held.pending_reads.insert(id, token);
        PendingRead { tracks: Arc::clone(tracks), id }
    };
    for _ in 0..2 {
        let before = store.sequence();
        let (answer, read) = run();
        let mut held = tracks.lock();
        if !held.pending_reads.contains_key(&pending.id) {
            return answer;
        }
        if before == store.sequence() {
            held.file(token, read);
            return answer;
        }
    }
    let mut held = tracks.lock();
    if !held.pending_reads.contains_key(&pending.id) {
        drop(held);
        return run().0;
    }
    let (answer, read) = run();
    held.file(token, read);
    answer
}

#[cfg(test)]
mod registration_tests {
    use super::*;
    use crate::store::StoreOptions;

    fn dependency() -> ReadSet {
        ReadSet { keys: [b"key".to_vec()].into(), ranges: Vec::new() }
    }

    #[test]
    fn an_unwinding_read_drops_its_pending_registration() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreOptions { syncs: false, ..Default::default() }).unwrap();
        let tracks = SharedTracks::default();
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = read_registered(&tracks, &store, 7, || panic!("read execution"));
            }))
            .is_err()
        );
        assert!(tracks.lock().is_empty());
        assert!(tracks.lock().pending_reads.is_empty());
    }

    #[test]
    fn a_watch_retries_a_concurrent_commit_and_honors_removal_during_execution() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreOptions { syncs: false, ..Default::default() }).unwrap();
        let tracks = SharedTracks::default();
        tracks.lock().watches.insert(
            1,
            Watching { reads: Reads { reads: Vec::new(), track: None }, last: None, listener: None, again: false },
        );
        let mut runs = 0;
        let (answer, changed) = read_watch_with(&tracks, &store, 1, |_| {
            assert!(tracks.try_lock().is_some());
            runs += 1;
            if runs == 1 {
                store.write(|_, _| Ok(()), false).unwrap();
            }
            (Ok(vec![runs]), dependency())
        })
        .unwrap();
        assert_eq!(answer.unwrap(), [2]);
        assert!(changed);
        assert_eq!(runs, 2);
        assert!(
            read_watch_with(&tracks, &store, 1, |_| {
                unwatch(&tracks, 1);
                (Ok(Vec::new()), dependency())
            })
            .is_none()
        );
        assert_eq!(tracks.lock().stats(), (0, 0, 0));
    }

    #[test]
    fn an_intervening_commit_retries_before_filing_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreOptions { syncs: false, ..Default::default() }).unwrap();
        let tracks = SharedTracks::default();
        let mut runs = 0;
        let answer = read_registered(&tracks, &store, 7, || {
            runs += 1;
            assert!(tracks.try_lock().is_some(), "ordinary execution releases the tracking lock");
            if runs == 1 {
                store.write(|_, _| Ok(()), false).unwrap();
            }
            (Ok(vec![runs]), dependency())
        })
        .unwrap();
        assert_eq!(answer, [2]);
        assert_eq!(runs, 2);
        assert!(tracks.lock().pending_reads.is_empty());
        let keys = [b"key".to_vec()].into();
        assert_eq!(tracks.lock().touched(Some(&keys)).tokens, [7]);
    }

    #[test]
    fn repeated_conflicts_fall_back_once_and_untrack_cancels_in_flight_registration() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreOptions { syncs: false, ..Default::default() }).unwrap();
        let tracks = SharedTracks::default();
        let mut runs = 0;
        read_registered(&tracks, &store, 7, || {
            runs += 1;
            if runs < 3 {
                assert!(tracks.try_lock().is_some());
                store.write(|_, _| Ok(()), false).unwrap();
            } else {
                assert!(tracks.try_lock().is_none(), "last attempt is serialized");
            }
            (Ok(Vec::new()), dependency())
        })
        .unwrap();
        assert_eq!(runs, 3);
        tracks.lock().untrack(7);
        read_registered(&tracks, &store, 7, || {
            tracks.lock().untrack(7);
            (Ok(Vec::new()), dependency())
        })
        .unwrap();
        assert_eq!(tracks.lock().stats(), (0, 0, 0));
        assert!(tracks.lock().pending_reads.is_empty());
    }
}
