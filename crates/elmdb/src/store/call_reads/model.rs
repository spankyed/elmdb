//! The registry under Shuttle's threads (`crate::sync::model`): reads asked while commits land, each run on a thread
//! of its own while a growth runs every read still waiting, with a reader table so small that reads share and wait for
//! their commit's transactions. The gate is Shuttle's lock, held as the store holds its own; LMDB is the stand-in below

use std::collections::BTreeMap;

use shuttle::sync::{Mutex, RwLock};

use super::{Asked, AtCommit, CallReads, Ticket, Work};
use crate::error::{Error, ErrorCode};
use crate::sync::model::{Arc, AtomicU64, Ordering, explore, thread};

/// The most transactions the stand-in's reader table holds, so a commit's reads share one and wait for it
const SLOTS: u64 = 2;

/// A transaction at `commit`, counted in `open` while it lives
#[derive(Debug)]
struct Txn {
    commit: u64,
    open: Arc<AtomicU64>,
}

impl AtCommit for Txn {
    fn read_at(&self) -> u64 {
        self.commit
    }
}

impl Drop for Txn {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
}

#[test]
fn every_read_runs_once_on_a_commit_of_its_call_and_every_transaction_ends() {
    explore(|| {
        let open = Arc::new(AtomicU64::new(0));
        let last = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(RwLock::new(()));
        let reads = std::sync::Arc::new(CallReads::<Txn>::default());
        // Each read's call, as the commits before and after it, and the commits it ran on
        let calls = Arc::new(Mutex::new(BTreeMap::<u32, (u64, u64)>::new()));
        let ran = Arc::new(Mutex::new(BTreeMap::<u32, Vec<u64>>::new()));

        let committer = {
            let last = Arc::clone(&last);
            thread::spawn(move || {
                for _ in 0..2 {
                    last.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        let grower = {
            let (gate, reads) = (Arc::clone(&gate), std::sync::Arc::clone(&reads));
            thread::spawn(move || {
                let _held = gate.write().unwrap();
                reads.run_all();
            })
        };
        let mut runners = Vec::new();
        for read in 0..3u32 {
            let before = last.load(Ordering::SeqCst);
            let work: Work<Txn> = {
                let ran = Arc::clone(&ran);
                Box::new(move |txn: &Txn| ran.lock().unwrap().entry(read).or_default().push(txn.commit))
            };
            let asked = {
                let _held = gate.read().unwrap();
                let opening = || {
                    if open.load(Ordering::SeqCst) >= SLOTS {
                        return Err(Error::new(ErrorCode::ReadersFull, "the stand-in's table is full"));
                    }
                    open.fetch_add(1, Ordering::SeqCst);
                    Ok((Txn { commit: last.load(Ordering::SeqCst), open: Arc::clone(&open) }, 0))
                };
                reads.ask(last.load(Ordering::SeqCst), opening, work)
            };
            let after = last.load(Ordering::SeqCst);
            let ticket: Ticket<Txn> = match asked {
                Ok(Asked::Waiting(ticket)) => ticket,
                Ok(Asked::Here(_)) => unreachable!("three reads hold no more than the bound"),
                // Its commit holds no transaction and the table is full: refused, and never run
                Err((error, _)) => {
                    assert_eq!(error.code, ErrorCode::ReadersFull);
                    continue;
                }
            };
            calls.lock().unwrap().insert(read, (before, after));
            let gate = Arc::clone(&gate);
            runners.push(thread::spawn(move || {
                let gate = &gate;
                ticket.run(|| gate.read().unwrap(), drop).unwrap();
            }));
        }
        for runner in runners {
            runner.join().unwrap();
        }
        committer.join().unwrap();
        grower.join().unwrap();

        let ran = ran.lock().unwrap();
        for (&read, &(before, after)) in calls.lock().unwrap().iter() {
            let commits = ran.get(&read).map_or(&[][..], Vec::as_slice);
            assert_eq!(commits.len(), 1, "read {read} ran on {commits:?}");
            let commit = commits[0];
            assert!(
                (before..=after).contains(&commit),
                "read {read} asked between {before} and {after} ran on {commit}"
            );
        }
        assert_eq!(ran.len(), calls.lock().unwrap().len(), "a refused read ran");
        assert_eq!(reads.held(), (0, 0));
        assert_eq!(open.load(Ordering::SeqCst), 0, "a transaction outlived every read");
    });
}
