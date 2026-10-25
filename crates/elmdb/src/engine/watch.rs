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
    listener: Listener,
}

/// The read sets filed, and the watches
#[derive(Default)]
pub struct Tracks {
    index: TrackIndex<Owner>,
    watches: BTreeMap<u64, Watching>,
    /// Watches ended while their reads ran, so the run doesn't put them back
    ended: HashSet<u64>,
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

/// Runs `reads` on a snapshot of the last commit, recording what they read
pub fn run_tracked(store: &Store, schema: &Schema, reads: &Reads) -> Result<(Result<Vec<u8>>, ReadSet)> {
    store.read(|txn, db| {
        let snapshot = Snapshot { txn, db, schema };
        let recording = Recording::new(&snapshot);
        let answer = super::reads::run(&recording, &reads.reads);
        Ok((answer, recording.into_reads()))
    })
}

/// Registers a watch of `reads` and delivers its first answer
pub fn watch(tracks: &SharedTracks, store: &Store, schema: &Schema, reads: Reads, listener: Listener) -> Result<u64> {
    let id = {
        let mut held = tracks.lock();
        let id = held.next;
        held.next += 1;
        held.watches.insert(id, Watching { reads, last: None, listener });
        id
    };
    deliver(tracks, store, schema, &[id])?;
    Ok(id)
}

/// Ends a watch: nothing is delivered to it after
pub fn unwatch(tracks: &SharedTracks, id: u64) {
    let mut held = tracks.lock();
    if held.watches.remove(&id).is_none() {
        held.ended.insert(id);
    }
    held.index.remove(&Owner::Watch(id));
}

/// Runs each watch again and hands its listener the answer when it differs from the last, outside the lock, so a
/// listener may read, write or end a watch itself
pub fn deliver(tracks: &SharedTracks, store: &Store, schema: &Schema, ids: &[u64]) -> Result<()> {
    for &id in ids {
        let Some(mut watching) = tracks.lock().watches.remove(&id) else { continue };
        let (answer, reads) = run_tracked(store, schema, &watching.reads)?;
        let changed = match &answer {
            Ok(bytes) => watching.last.as_ref() != Some(bytes),
            Err(_) => true,
        };
        if let Ok(bytes) = &answer {
            watching.last = Some(bytes.clone());
        }
        {
            let mut held = tracks.lock();
            if held.ended.remove(&id) {
                continue;
            }
            held.index.remove(&Owner::Watch(id));
            held.index.add(&Owner::Watch(id), reads);
        }
        if changed {
            (watching.listener)(&answer);
        }
        let mut held = tracks.lock();
        if held.ended.remove(&id) {
            held.index.remove(&Owner::Watch(id));
        } else {
            held.watches.insert(id, watching);
        }
    }
    Ok(())
}
