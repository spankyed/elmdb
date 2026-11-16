//! Reads asked for on one thread and run on another, each from the snapshot of the last commit as it was when it was
//! asked for (`docs/architecture/engine.md`, Reads): a read transaction is taken at the call and held, outside the gate
//! as an idle one is, until a thread runs the read on it. A commit made meanwhile, here or in another process, changes
//! nothing the read sees.
//!
//! What it costs is bounded. The reads asked at one commit share its transactions, at most `PER_COMMIT` of them and
//! each used by one read at a time, so a burst of reads takes a few of LMDB's reader slots rather than one each. No more
//! than `HELD` are held in all: a read asked at a commit that has none, once that many are held, is run where it was
//! asked. A growth of the map, and the adoption of a size another process grew it to, which no transaction of the
//! process may outlast, first runs every read still waiting, each on a transaction of its own commit, and then ends
//! them; a close ends them, and a read still waiting is refused `CLOSED`. No read is ever run on a later snapshot than
//! its call's.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, ErrorCode, Result};
use crate::sync::{PlainCondvar, PlainMutex};

/// The most transactions the reads asked at one commit share: as many as libuv's pool runs at once unless told more,
/// so a burst of reads asked together runs as it would on transactions of its own
pub(crate) const PER_COMMIT: usize = 4;

/// The most transactions held for reads in all. LMDB's reader slots, 126 (its default, which elmdb doesn't change),
/// are shared by every transaction of every process with the database open, of which a read on the calling thread, the
/// pool's reads and the idle snapshots (`MAX_IDLE`) take some already
pub(crate) const HELD: usize = 16;

/// A read transaction, as the registry knows it: the commit it reads
pub(crate) trait AtCommit: Send {
    /// The commit it reads, LMDB's id of it
    fn read_at(&self) -> u64;
}

/// A read, run on a transaction at the commit it was asked at. It answers through what it captured, and doesn't unwind
pub(crate) type Work<T> = Box<dyn FnOnce(&T) + Send>;

/// One commit's transactions and the reads asked at it
struct Commit<T> {
    /// The transactions no read is using, each with the idle cache's generation it was opened in
    free: Vec<(T, u64)>,
    /// How many transactions the commit has, free or in use
    opened: usize,
    /// The reads asked at the commit that haven't finished, begun or not
    waiting: usize,
}

struct State<T> {
    commits: BTreeMap<u64, Commit<T>>,
    /// The reads not yet begun, by ticket, each with the commit it was asked at
    reads: BTreeMap<u64, (u64, Work<T>)>,
    next: u64,
    /// The transactions held, every commit's
    held: usize,
    /// Closed: every transaction is ended, and every read still waiting refused
    closed: bool,
}

impl<T: AtCommit> State<T> {
    /// Holds `txn` for the reads of its commit, answering that commit
    fn hold(&mut self, txn: T, generation: u64) -> u64 {
        let at = txn.read_at();
        let commit = self.commits.entry(at).or_insert_with(|| Commit { free: Vec::new(), opened: 0, waiting: 0 });
        commit.free.push((txn, generation));
        commit.opened += 1;
        self.held += 1;
        at
    }

    /// A read asked at `at` has finished, or won't run: the commit's transactions, all free once it has no read left,
    /// are let go of and answered, for the idle cache
    fn finished(&mut self, at: u64) -> Vec<(T, u64)> {
        let Some(commit) = self.commits.get_mut(&at) else { return Vec::new() };
        commit.waiting -= 1;
        if commit.waiting > 0 {
            return Vec::new();
        }
        let commit = self.commits.remove(&at).expect("found above");
        self.held -= commit.opened;
        commit.free
    }
}

/// The reads asked for and not yet run, and the transactions held for them
pub(crate) struct CallReads<T> {
    state: PlainMutex<State<T>>,
    /// A transaction was handed back, or every read was run or refused
    returned: PlainCondvar,
    /// Closed at exit, the lock tried and not waited for: no close after waits for it
    exited: AtomicBool,
}

/// What asking for a read came to
pub(crate) enum Asked<T: AtCommit> {
    /// The read waits for `Ticket::run`
    Waiting(Ticket<T>),
    /// As many transactions are held as may be, and none at the last commit: the read is the caller's to run, now
    Here(Work<T>),
}

/// A read waiting to run. Dropped without running, it won't: its commit's transactions are let go of once no read of
/// theirs is left
pub(crate) struct Ticket<T: AtCommit> {
    reads: Arc<CallReads<T>>,
    ticket: Option<u64>,
}

fn closed() -> Error {
    Error::new(ErrorCode::Closed, "The database closed before a read asked of it ran")
}

impl<T: AtCommit> Default for CallReads<T> {
    fn default() -> Self {
        Self {
            state: PlainMutex::new(State {
                commits: BTreeMap::new(),
                reads: BTreeMap::new(),
                next: 0,
                held: 0,
                closed: false,
            }),
            returned: PlainCondvar::default(),
            exited: AtomicBool::new(false),
        }
    }
}

impl<T: AtCommit> CallReads<T> {
    /// Asks for `work` to run on the snapshot of `last`, the last commit, which `open` opens a transaction at. The
    /// caller holds the gate for reading, so no growth ends a transaction between its opening and its holding here. A
    /// transaction `open` opens at a later commit, one that landed since `last` was read, is the read's: that commit
    /// landed before the call returned. A failure to open one hands the work back, for the caller to ask again once it
    /// has done what the failure asks (adopt a grown map); a full reader table is no failure where the commit holds a
    /// transaction already, which the read then waits its turn for
    pub(crate) fn ask(
        self: &Arc<Self>,
        last: u64,
        open: impl FnOnce() -> Result<(T, u64)>,
        work: Work<T>,
    ) -> std::result::Result<Asked<T>, (Error, Work<T>)> {
        let mut state = self.state.lock();
        if state.closed {
            return Err((closed(), work));
        }
        let room = state.held < HELD;
        let at = match state.commits.get(&last) {
            // A transaction of the commit is spare, or it has as many as it may: the read shares them
            Some(commit) if commit.opened > commit.waiting || commit.opened >= PER_COMMIT || !room => last,
            Some(_) => match open() {
                Ok((txn, generation)) => state.hold(txn, generation),
                Err(error) if error.code == ErrorCode::ReadersFull => last,
                Err(error) => return Err((error, work)),
            },
            None if !room => return Ok(Asked::Here(work)),
            None => match open() {
                Ok((txn, generation)) => state.hold(txn, generation),
                Err(error) => return Err((error, work)),
            },
        };
        let ticket = state.next;
        state.next += 1;
        state.reads.insert(ticket, (at, work));
        state.commits.get_mut(&at).expect("held above").waiting += 1;
        Ok(Asked::Waiting(Ticket { reads: Arc::clone(self), ticket: Some(ticket) }))
    }

    /// Runs every read still waiting on a transaction of its commit, then ends every transaction: what a growth of the
    /// map, and the adoption of a size another process grew it to, does first. The caller holds the gate for writing,
    /// so no read is using a transaction, and none is asked for, until it is done
    pub(crate) fn run_all(&self) {
        let mut state = self.state.lock();
        let reads = std::mem::take(&mut state.reads);
        for (at, work) in reads.into_values() {
            let commit = state.commits.get(&at).expect("a waiting read's commit holds its transactions");
            let (txn, _) = commit.free.last().expect("no read uses a transaction while the gate is held for writing");
            // A read answers through what it captured, catching its own panic; one that unwound anyway is left
            // unanswered, which its ticket's holder reports, rather than ending the growth that ran it
            let _ = catch_unwind(AssertUnwindSafe(|| work(txn)));
        }
        state.commits.clear();
        state.held = 0;
        drop(state);
        self.returned.notify_all();
    }

    /// Ends every transaction, and refuses `CLOSED` every read still waiting: what a close does before the environment
    /// closes, which no transaction may outlast
    pub(crate) fn close(&self) {
        if self.exited.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock();
        Self::end(&mut state);
        drop(state);
        self.returned.notify_all();
    }

    /// `close` in a process that is exiting, whose other threads may be gone holding the lock: tried until `deadline`,
    /// and no `close` after waits for it
    pub(crate) fn close_before(&self, deadline: std::time::Instant) {
        self.exited.store(true, Ordering::Release);
        if let Some(mut state) = self.state.try_lock_until(deadline) {
            Self::end(&mut state);
        }
    }

    fn end(state: &mut State<T>) {
        state.closed = true;
        state.reads.clear();
        state.commits.clear();
        state.held = 0;
    }

    /// Hands back a transaction a read has finished with, answering the commit's transactions once it has no read left
    fn give_back(&self, at: u64, txn: (T, u64)) -> Vec<(T, u64)> {
        let mut state = self.state.lock();
        if let Some(commit) = state.commits.get_mut(&at) {
            commit.free.push(txn);
        }
        let freed = state.finished(at);
        drop(state);
        self.returned.notify_all();
        freed
    }

    /// How many transactions are held, every commit's, and the most any one commit holds
    #[cfg(test)]
    pub(crate) fn held(&self) -> (usize, usize) {
        let state = self.state.lock();
        (state.held, state.commits.values().map(|commit| commit.opened).max().unwrap_or(0))
    }
}

impl<T: AtCommit> Ticket<T> {
    /// Whether the read was asked of `reads`
    pub(crate) fn of(&self, reads: &Arc<CallReads<T>>) -> bool {
        Arc::ptr_eq(&self.reads, reads)
    }

    /// Runs the read on a transaction of the commit it was asked at, unless a growth ran it first, handing `retire` the
    /// transactions its commit no longer needs once it was the last of the commit's reads, for the idle cache. `hold`
    /// takes the gate for reading: a read uses a transaction only while the gate is held, so a growth, which holds it
    /// for writing, finds every one free, and `retire` is handed them with it held, so none is ended beside a growth.
    /// While every transaction of the commit is in use by another of its reads, each running, this waits for one to be
    /// handed back. Refused `CLOSED` once the reads closed
    pub(crate) fn run<G>(self, hold: impl Fn() -> G, retire: impl FnOnce(Vec<(T, u64)>)) -> Result<()> {
        // Kept on the ticket until it is dropped, which lets the read go if this unwinds before it is taken to run
        let ticket = self.ticket.expect("a ticket is dropped once it has run");
        let reads = &self.reads;
        loop {
            let gate = hold();
            let mut state = reads.state.lock();
            if state.closed {
                return Err(closed());
            }
            let Some(&(at, _)) = state.reads.get(&ticket) else { return Ok(()) };
            let commit = state.commits.get_mut(&at).expect("a waiting read's commit holds its transactions");
            if let Some(txn) = commit.free.pop() {
                let (_, work) = state.reads.remove(&ticket).expect("found above");
                drop(state);
                let mut using = Using { reads, at, txn: Some(txn) };
                work(&using.txn.as_ref().expect("held until handed back").0);
                retire(using.hand_back());
                drop(gate);
                return Ok(());
            }
            // The gate is let go of before the wait, so a growth can run this read meanwhile
            drop(gate);
            drop(reads.returned.wait(state));
        }
    }
}

impl<T: AtCommit> Drop for Ticket<T> {
    /// A read run, by its ticket or a growth, or refused by a close, is gone already, and this lets nothing go
    fn drop(&mut self) {
        let Some(ticket) = self.ticket.take() else { return };
        let mut state = self.reads.state.lock();
        if let Some((at, _)) = state.reads.remove(&ticket) {
            // Ended with the lock held, which a growth takes before it resizes the map: none is ended beside it
            drop(state.finished(at));
        }
    }
}

/// A transaction a read is using, handed back when it is done, or when it unwinds
struct Using<'r, T: AtCommit> {
    reads: &'r CallReads<T>,
    at: u64,
    txn: Option<(T, u64)>,
}

impl<T: AtCommit> Using<'_, T> {
    fn hand_back(&mut self) -> Vec<(T, u64)> {
        match self.txn.take() {
            Some(txn) => self.reads.give_back(self.at, txn),
            None => Vec::new(),
        }
    }
}

impl<T: AtCommit> Drop for Using<'_, T> {
    /// A read that unwound: its run's gate is still held, as the gate is let go of after this
    fn drop(&mut self) {
        drop(self.hand_back());
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, shuttle))]
mod model;
