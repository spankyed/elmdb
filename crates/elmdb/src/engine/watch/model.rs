//! Watch delivery and token registration under Shuttle's threads (`crate::sync::model`), against a store of one key
//! holding a number. Delivery, registration and the tracking lock are this module's own code; the store's reads, its
//! sequence and LMDB are the stand-in below, and a commit does what `finish_commit` does after one

use std::collections::BTreeSet;

use super::{Listener, SharedTracks, deliver_with, read_registered, read_watch_with, register, unwatch};
use crate::engine::plan::Reads;
use crate::engine::tracking::ReadSet;
use crate::error::Result;
use crate::sync::Mutex;
use crate::sync::model::{Arc, AtomicBool, AtomicU64, Ordering, explore, thread};

const KEY: &[u8] = b"key";

/// One key's number and the last commit's sequence, raised with it
struct Store {
    value: Mutex<u64>,
    sequence: AtomicU64,
}

impl Store {
    fn new() -> Arc<Self> {
        Arc::new(Self { value: Mutex::new(0), sequence: AtomicU64::new(0) })
    }

    fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::SeqCst)
    }

    fn read(&self) -> (Result<Vec<u8>>, ReadSet) {
        let value = *self.value.lock().unwrap();
        (Ok(value.to_be_bytes().to_vec()), ReadSet { keys: BTreeSet::from([KEY.to_vec()]), ranges: Vec::new() })
    }

    fn deliver(&self, tracks: &SharedTracks, ids: &[u64]) {
        let panicked =
            deliver_with(tracks, ids, |id| read_watch_with(tracks, &|| self.sequence(), id, |_| self.read()));
        assert!(panicked.is_empty());
    }

    /// Writes `value` and commits, then tells who it touched as `finish_commit` does, answering the tokens it named
    fn commit(&self, tracks: &SharedTracks, value: u64) -> Vec<u32> {
        {
            let mut held = self.value.lock().unwrap();
            *held = value;
            self.sequence.fetch_add(1, Ordering::SeqCst);
        }
        let touched = tracks.lock().touched(Some(&[KEY.to_vec()].into()));
        self.deliver(tracks, &touched.watches);
        touched.tokens
    }

    /// Files a watch and delivers its first answer, as `watch` does
    fn watch(&self, tracks: &SharedTracks, listener: Listener) -> u64 {
        let id = register(tracks, Reads { reads: Vec::new(), track: None }, listener);
        self.deliver(tracks, &[id]);
        id
    }
}

fn number(answer: &Result<Vec<u8>>) -> u64 {
    u64::from_be_bytes(answer.as_ref().unwrap().as_slice().try_into().unwrap())
}

/// Counts its drops, the listener's captures going with it
struct Dropped(Arc<AtomicU64>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Two commits deliver at once: whichever finds the listener out marks the watch to run again, so the last answer
/// the listener hears is the last commit's, and no answer is the one before it again
#[test]
fn commits_delivering_together_leave_the_listener_with_the_last_answer() {
    explore(|| {
        let store = Store::new();
        let tracks = SharedTracks::default();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let listener = {
            let heard = heard.clone();
            Box::new(move |answer: &Result<Vec<u8>>| heard.lock().unwrap().push(number(answer)))
        };
        let id = store.watch(&tracks, listener);
        let committers: Vec<_> = (1..=2)
            .map(|value| {
                let (store, tracks) = (store.clone(), tracks.clone());
                thread::spawn(move || store.commit(&tracks, value))
            })
            .collect();
        for committer in committers {
            committer.join().unwrap();
        }
        let heard = heard.lock().unwrap();
        let last = *store.value.lock().unwrap();
        assert_eq!(heard.last(), Some(&last), "the last answer heard isn't the last commit's: {heard:?}");
        assert!(heard.windows(2).all(|pair| pair[0] != pair[1]), "an answer was delivered twice: {heard:?}");
        assert!(tracks.lock().watches[&id].listener.is_some(), "the listener wasn't put back");
    });
}

/// A watch ended from another thread while a commit delivers: nothing is delivered once `unwatch` has returned, the
/// listener is dropped once, and nothing stays filed
#[test]
fn a_watch_ended_during_delivery_hears_nothing_after_and_is_dropped_once() {
    explore(|| {
        let store = Store::new();
        let tracks = SharedTracks::default();
        let ended = Arc::new(AtomicBool::new(false));
        let late = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicU64::new(0));
        let listener = {
            let (ended, late, dropped) = (ended.clone(), late.clone(), Dropped(drops.clone()));
            Box::new(move |_: &Result<Vec<u8>>| {
                let _owned = &dropped;
                if ended.load(Ordering::SeqCst) {
                    late.store(true, Ordering::SeqCst);
                }
            })
        };
        let id = store.watch(&tracks, listener);
        let committer = {
            let (store, tracks) = (store.clone(), tracks.clone());
            thread::spawn(move || store.commit(&tracks, 1))
        };
        unwatch(&tracks, id);
        ended.store(true, Ordering::SeqCst);
        committer.join().unwrap();
        assert!(!late.load(Ordering::SeqCst), "the listener was called after unwatch returned");
        assert_eq!(drops.load(Ordering::SeqCst), 1, "the listener wasn't dropped exactly once");
        assert_eq!(tracks.lock().stats(), (0, 0, 0));
        assert!(tracks.lock().watches.is_empty());
    });
}

/// A listener that ends its own watch while other commits deliver: it is dropped once, and nothing stays filed
#[test]
fn a_listener_ending_its_own_watch_is_dropped_once() {
    explore(|| {
        let store = Store::new();
        let tracks = SharedTracks::default();
        let drops = Arc::new(AtomicU64::new(0));
        let own = Arc::new(AtomicU64::new(u64::MAX));
        let listener = {
            let (tracks, own, dropped) = (tracks.clone(), own.clone(), Dropped(drops.clone()));
            Box::new(move |answer: &Result<Vec<u8>>| {
                let _owned = &dropped;
                if number(answer) == 1 {
                    unwatch(&tracks, own.load(Ordering::SeqCst));
                }
            })
        };
        own.store(store.watch(&tracks, listener), Ordering::SeqCst);
        let committers: Vec<_> = (1..=2)
            .map(|value| {
                let (store, tracks) = (store.clone(), tracks.clone());
                thread::spawn(move || store.commit(&tracks, value))
            })
            .collect();
        for committer in committers {
            committer.join().unwrap();
        }
        let filed = tracks.lock().watches.len();
        if filed == 0 {
            assert_eq!(drops.load(Ordering::SeqCst), 1, "an ended watch's listener wasn't dropped exactly once");
            assert_eq!(tracks.lock().stats(), (0, 0, 0));
        } else {
            // The listener heard 2 last and never 1: the watch is still filed, its listener held
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }
    });
}

/// Two listeners, each ending the other's watch, delivered by two commits at once: neither waits on the other's
/// delivery, which would leave both waiting
#[test]
fn listeners_ending_each_others_watch_do_not_wait_on_each_other() {
    explore(|| {
        let store = Store::new();
        let tracks = SharedTracks::default();
        let ids: Arc<[AtomicU64; 2]> = Arc::new([AtomicU64::new(u64::MAX), AtomicU64::new(u64::MAX)]);
        for (mine, other) in [(0, 1), (1, 0)] {
            let listener = {
                let (tracks, ids) = (tracks.clone(), ids.clone());
                Box::new(move |answer: &Result<Vec<u8>>| {
                    if number(answer) != 0 {
                        unwatch(&tracks, ids[other].load(Ordering::SeqCst));
                    }
                })
            };
            ids[mine].store(store.watch(&tracks, listener), Ordering::SeqCst);
        }
        let committers: Vec<_> = (1..=2)
            .map(|value| {
                let (store, tracks) = (store.clone(), tracks.clone());
                thread::spawn(move || store.commit(&tracks, value))
            })
            .collect();
        for committer in committers {
            committer.join().unwrap();
        }
        assert!(tracks.lock().watches.len() < 2, "neither listener ended the other's watch");
    });
}

/// A token's read races a commit: the commit names the token, or the read already saw what it wrote
#[test]
fn a_commit_racing_a_tracked_read_names_its_token_or_the_read_saw_it() {
    explore(|| {
        let store = Store::new();
        let tracks = SharedTracks::default();
        let reader = {
            let (store, tracks) = (store.clone(), tracks.clone());
            thread::spawn(move || read_registered(&tracks, &|| store.sequence(), 7, || store.read()))
        };
        let named = store.commit(&tracks, 1);
        let answer = reader.join().unwrap();
        assert!(named.contains(&7) || number(&answer) == 1, "the commit missed a read that didn't see it");
    });
}
