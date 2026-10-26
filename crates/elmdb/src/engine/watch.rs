//! What commits tell tracked reads (`docs/architecture/engine.md`, Change sets and watch): the read sets filed under a
//! caller's tokens and under the database's own watches, and the watches, each a set of reads run again after every
//! commit that wrote into what it read last, its listener handed each answer that differs from the one before.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::Result;
use crate::schema::Schema;
use crate::store::Store;

use super::plan::Reads;
use super::tracking::{ReadSet, Recording, TrackIndex};
use super::unit::Snapshot;

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
        self.index.is_empty()
    }

    pub fn file(&mut self, token: u32, reads: ReadSet) {
        self.index.add(&Owner::Token(token), reads);
    }

    pub fn untrack(&mut self, token: u32) {
        self.index.remove(&Owner::Token(token));
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
pub fn run_tracked(store: &Store, schema: &Schema, reads: &Reads) -> (Result<Vec<u8>>, ReadSet) {
    let ran = store.read(|txn, db| {
        let snapshot = Snapshot { txn, db, schema };
        let recording = Recording::new(&snapshot);
        let answer = super::reads::run(&recording, &reads.reads);
        Ok((answer, recording.into_reads()))
    });
    ran.unwrap_or_else(|error| (Err(error), ReadSet { keys: BTreeSet::new(), ranges: vec![(Vec::new(), None)] }))
}

/// Registers a watch of `reads` and delivers its first answer
pub fn watch(tracks: &SharedTracks, store: &Store, schema: &Schema, reads: Reads, listener: Listener) -> Result<u64> {
    let id = {
        let mut held = tracks.lock();
        let id = held.next;
        held.next += 1;
        held.watches.insert(id, Watching { reads, last: None, listener: Some(listener), again: false });
        id
    };
    deliver(tracks, store, schema, &[id]);
    Ok(id)
}

/// Ends a watch: nothing is delivered to it after
pub fn unwatch(tracks: &SharedTracks, id: u64) {
    let mut held = tracks.lock();
    held.watches.remove(&id);
    held.index.remove(&Owner::Watch(id));
}

/// Runs each watch again and hands its listener the answer when it differs from the last. The read runs and its
/// read set is filed under the lock commits name tokens under, so a commit either is in what the read saw or names
/// the watch; the listener runs outside it, so it may read, write or end a watch itself. A watch a commit names while
/// its listener runs is run again once the listener returns. A read that fails is handed to the listener as such
pub fn deliver(tracks: &SharedTracks, store: &Store, schema: &Schema, ids: &[u64]) {
    for &id in ids {
        let mut listener = {
            let mut held = tracks.lock();
            let Some(watching) = held.watches.get_mut(&id) else { continue };
            match watching.listener.take() {
                Some(listener) => listener,
                None => {
                    watching.again = true;
                    continue;
                }
            }
        };
        loop {
            let (answer, changed) = {
                let mut held = tracks.lock();
                let Some(watching) = held.watches.get(&id) else { break };
                let reads = watching.reads.clone();
                let (answer, read) = run_tracked(store, schema, &reads);
                held.index.remove(&Owner::Watch(id));
                held.index.add(&Owner::Watch(id), read);
                let watching = held.watches.get_mut(&id).expect("held under the lock");
                watching.again = false;
                let changed = match &answer {
                    Ok(bytes) => watching.last.as_ref() != Some(bytes),
                    Err(_) => true,
                };
                // A failure is what the listener last heard, so the next answer is delivered whatever it holds
                watching.last = answer.as_ref().ok().cloned();
                (answer, changed)
            };
            if changed {
                listener(&answer);
            }
            let mut held = tracks.lock();
            // Ended while its listener ran: its read set went with it
            let Some(watching) = held.watches.get_mut(&id) else {
                held.index.remove(&Owner::Watch(id));
                break;
            };
            if !watching.again {
                watching.listener = Some(listener);
                break;
            }
        }
    }
}

/// Runs `reads` tracked under `token`, filing what they read before any commit can name what they read: the snapshot
/// is taken and the read set filed under the lock commits name tokens under
pub fn read_tracked(
    tracks: &SharedTracks,
    store: &Store,
    schema: &Schema,
    reads: &Reads,
    token: u32,
) -> Result<Vec<u8>> {
    let mut held = tracks.lock();
    let (answer, read) = run_tracked(store, schema, reads);
    held.file(token, read);
    answer
}
