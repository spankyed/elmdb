//! The registry over a stand-in transaction that counts the ones open: which commit each read runs on, what a commit's
//! reads share, the bound on what is held, and what a growth, a close and a dropped ticket leave

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use super::*;

/// A transaction at `commit`, counted in `open` while it lives
#[derive(Debug)]
struct Txn {
    commit: u64,
    open: Arc<AtomicUsize>,
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

/// The transactions open, and the commit a new one opens at
#[derive(Clone, Default)]
struct Env {
    open: Arc<AtomicUsize>,
    last: Arc<AtomicUsize>,
}

impl Env {
    fn last(&self) -> u64 {
        self.last.load(Ordering::SeqCst) as u64
    }

    fn commit(&self) {
        self.last.fetch_add(1, Ordering::SeqCst);
    }

    fn open(&self) -> Result<(Txn, u64)> {
        self.open.fetch_add(1, Ordering::SeqCst);
        Ok((Txn { commit: self.last(), open: Arc::clone(&self.open) }, 0))
    }

    fn open_now(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }
}

/// Asks for a read that records the commit it ran on in `saw`
fn ask(reads: &Arc<CallReads<Txn>>, env: &Env, saw: &Arc<Mutex<Vec<u64>>>) -> Ticket<Txn> {
    let saw = Arc::clone(saw);
    let work: Work<Txn> = Box::new(move |txn: &Txn| saw.lock().unwrap().push(txn.commit));
    match reads.ask(env.last(), || env.open(), work) {
        Ok(Asked::Waiting(ticket)) => ticket,
        Ok(Asked::Here(_)) => panic!("asked to run here"),
        Err((error, _)) => panic!("refused: {error}"),
    }
}

#[test]
fn reads_asked_at_one_commit_share_at_most_a_few_transactions_and_let_them_go_once_run() {
    let (env, reads, saw) = (Env::default(), Arc::new(CallReads::default()), Arc::default());
    let tickets: Vec<_> = (0..10).map(|_| ask(&reads, &env, &saw)).collect();
    assert_eq!(reads.held(), (PER_COMMIT, PER_COMMIT));
    assert_eq!(env.open_now(), PER_COMMIT);
    let mut freed = Vec::new();
    for ticket in tickets {
        ticket.run(|| (), |retired| freed.extend(retired)).unwrap();
    }
    assert_eq!(*saw.lock().unwrap(), [0; 10]);
    assert_eq!(reads.held(), (0, 0));
    // Handed back for the idle cache once the commit's last read ran
    assert_eq!(freed.len(), PER_COMMIT);
    drop(freed);
    assert_eq!(env.open_now(), 0);
}

#[test]
fn a_read_asked_after_a_commit_reads_that_commit_and_one_asked_before_it_does_not() {
    let (env, reads, saw) = (Env::default(), Arc::new(CallReads::default()), Arc::default());
    let before = ask(&reads, &env, &saw);
    // A second read at the first commit, run already, leaves that commit a transaction to spare
    let spare = ask(&reads, &env, &saw);
    spare.run(|| (), drop).unwrap();
    assert_eq!(reads.held(), (2, 2));
    env.commit();
    let after = ask(&reads, &env, &saw);
    // Run in the other order: each reads its call's commit whichever runs first
    after.run(|| (), drop).unwrap();
    before.run(|| (), drop).unwrap();
    assert_eq!(*saw.lock().unwrap(), [0, 1, 0]);
}

#[test]
fn a_read_asked_at_a_commit_with_none_held_runs_here_once_as_many_are_held_as_may_be() {
    let (env, reads, saw) = (Env::default(), Arc::new(CallReads::default()), Arc::default());
    let mut tickets = Vec::new();
    for _ in 0..HELD {
        tickets.push(ask(&reads, &env, &saw));
        env.commit();
    }
    assert_eq!(reads.held(), (HELD, 1));
    let work: Work<Txn> = Box::new(|_| {});
    assert!(matches!(reads.ask(env.last(), || env.open(), work), Ok(Asked::Here(_))));
    // A commit that holds one already is still shared
    let work: Work<Txn> = Box::new(|_| {});
    assert!(matches!(reads.ask(env.last() - 1, || env.open(), work), Ok(Asked::Waiting(_))));
    assert_eq!(env.open_now(), HELD);
}

#[test]
fn a_full_reader_table_leaves_a_read_to_share_its_commit_s_transactions_and_refuses_one_whose_commit_has_none() {
    let (env, reads, saw) = (Env::default(), Arc::new(CallReads::default()), Arc::default());
    let first = ask(&reads, &env, &saw);
    let full = || Err(Error::new(ErrorCode::ReadersFull, "full"));
    let work: Work<Txn> = Box::new(|_| {});
    let Ok(Asked::Waiting(second)) = reads.ask(env.last(), full, work) else { panic!("refused") };
    assert_eq!(reads.held(), (1, 1));
    env.commit();
    let work: Work<Txn> = Box::new(|_| {});
    let Err((refused, _)) = reads.ask(env.last(), full, work) else { panic!("asked") };
    assert_eq!(refused.code, ErrorCode::ReadersFull);
    first.run(|| (), drop).unwrap();
    second.run(|| (), drop).unwrap();
    assert_eq!(env.open_now(), 0);
}

#[test]
fn a_growth_runs_each_waiting_read_on_its_own_commit_and_ends_every_transaction() {
    let (env, reads, saw) = (Env::default(), Arc::new(CallReads::default()), Arc::default());
    let first = ask(&reads, &env, &saw);
    env.commit();
    let second = ask(&reads, &env, &saw);
    reads.run_all();
    assert_eq!(*saw.lock().unwrap(), [0, 1]);
    assert_eq!(env.open_now(), 0);
    assert_eq!(reads.held(), (0, 0));
    // Run already: neither runs again
    first.run(|| (), |_| panic!("nothing to retire")).unwrap();
    second.run(|| (), |_| panic!("nothing to retire")).unwrap();
    assert_eq!(saw.lock().unwrap().len(), 2);
}

#[test]
fn a_close_ends_every_transaction_and_refuses_the_reads_still_waiting() {
    let (env, reads, saw) = (Env::default(), Arc::new(CallReads::default()), Arc::default());
    let waiting = ask(&reads, &env, &saw);
    reads.close();
    assert_eq!(env.open_now(), 0);
    assert_eq!(waiting.run(|| (), drop).unwrap_err().code, ErrorCode::Closed);
    assert!(saw.lock().unwrap().is_empty());
    let work: Work<Txn> = Box::new(|_| {});
    assert!(reads.ask(env.last(), || env.open(), work).is_err_and(|(error, _)| error.code == ErrorCode::Closed));
}

#[test]
fn a_ticket_dropped_unrun_lets_its_commit_s_transactions_go_once_no_read_of_theirs_is_left() {
    let (env, reads, saw) = (Env::default(), Arc::new(CallReads::default()), Arc::default());
    let (first, second) = (ask(&reads, &env, &saw), ask(&reads, &env, &saw));
    drop(first);
    assert_eq!(reads.held(), (2, 2));
    drop(second);
    assert_eq!(reads.held(), (0, 0));
    assert_eq!(env.open_now(), 0);
    assert!(saw.lock().unwrap().is_empty());
}

#[test]
fn a_read_that_unwinds_hands_its_transaction_back() {
    let (env, reads, saw) = (Env::default(), Arc::new(CallReads::default()), Arc::default());
    let work: Work<Txn> = Box::new(|_| panic!("a read's own"));
    let Ok(Asked::Waiting(panicking)) = reads.ask(env.last(), || env.open(), work) else { panic!("refused") };
    let other = ask(&reads, &env, &saw);
    assert!(std::panic::catch_unwind(AssertUnwindSafe(|| panicking.run(|| (), drop))).is_err());
    assert_eq!(reads.held(), (2, 2));
    other.run(|| (), drop).unwrap();
    assert_eq!(*saw.lock().unwrap(), [0]);
    assert_eq!(env.open_now(), 0);
}

#[test]
fn a_read_waits_while_every_transaction_of_its_commit_is_in_use_and_runs_once_one_is_handed_back() {
    let (env, reads, saw) = (Env::default(), Arc::new(CallReads::default()), Arc::<Mutex<Vec<u64>>>::default());
    // One transaction for both: the table is full for the second
    let barrier = Arc::new(Barrier::new(2));
    let holding: Work<Txn> = {
        let barrier = Arc::clone(&barrier);
        Box::new(move |_| {
            barrier.wait();
            barrier.wait();
        })
    };
    let Ok(Asked::Waiting(first)) = reads.ask(env.last(), || env.open(), holding) else { panic!("refused") };
    let saw_second = Arc::clone(&saw);
    let work: Work<Txn> = Box::new(move |txn: &Txn| saw_second.lock().unwrap().push(txn.commit));
    let full = || Err(Error::new(ErrorCode::ReadersFull, "full"));
    let Ok(Asked::Waiting(second)) = reads.ask(env.last(), full, work) else { panic!("refused") };
    let running = std::thread::spawn(move || first.run(|| (), drop).unwrap());
    barrier.wait();
    let waiting = std::thread::spawn(move || second.run(|| (), drop).unwrap());
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(saw.lock().unwrap().is_empty(), "the second read ran while its commit's one transaction was in use");
    barrier.wait();
    running.join().unwrap();
    waiting.join().unwrap();
    assert_eq!(*saw.lock().unwrap(), [0]);
    assert_eq!(env.open_now(), 0);
}

#[test]
fn a_close_at_exit_waits_no_longer_than_its_deadline_for_the_lock_and_no_close_after_waits_for_it() {
    let (env, reads, saw) = (Env::default(), Arc::new(CallReads::default()), Arc::default());
    // A growth's run of a waiting read holds the lock until the barrier lets it go, as a thread gone holding it would
    let barrier = Arc::new(Barrier::new(2));
    let holding: Work<Txn> = {
        let barrier = Arc::clone(&barrier);
        Box::new(move |_| {
            barrier.wait();
            barrier.wait();
        })
    };
    let Ok(Asked::Waiting(_held)) = reads.ask(env.last(), || env.open(), holding) else { panic!("refused") };
    let _other = ask(&reads, &env, &saw);
    let growing = {
        let reads = Arc::clone(&reads);
        std::thread::spawn(move || reads.run_all())
    };
    barrier.wait();
    let (closed, done) = std::sync::mpsc::channel();
    let closing = Arc::clone(&reads);
    std::thread::spawn(move || {
        closing.close_before(std::time::Instant::now() + std::time::Duration::from_millis(20));
        closing.close();
        closed.send(()).unwrap();
    });
    let returned = done.recv_timeout(std::time::Duration::from_secs(10));
    // The growth lets the lock go before anything is asserted, so a failure ends the test rather than its drops waiting
    barrier.wait();
    growing.join().unwrap();
    returned.expect("the closes returned while the lock was held");
}
