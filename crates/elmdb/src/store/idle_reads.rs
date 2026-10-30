//! Completed read transactions are reusable for a short burst, then released even if no caller reads again.
//! Active transactions never live here. One worker owns only the shared cache, never a Store or its map gate.
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use heed::{Env, RoTxn, WithoutTls};

const RETAIN_FOR: Duration = Duration::from_millis(100);

struct Entry {
    txn: RoTxn<'static, WithoutTls>,
    expires: Instant,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    generation: u64,
    stopping: bool,
    /// Once armed, the worker keeps this deadline even if a reader briefly takes the last entry.
    /// Otherwise every hot read could wake a sleeping worker, paying a context switch per operation.
    sweep: Option<Instant>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

pub(super) struct IdleReads {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl IdleReads {
    pub fn new() -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new().name("elmdb-read-expiry".to_owned()).spawn(move || expire(&shared))?
        };
        Ok(Self { shared, worker: Some(worker) })
    }

    /// The generation also belongs to a newly acquired transaction when the cache answers none.
    /// A local commit changes it, so a read completing afterwards cannot reinsert an older snapshot.
    pub fn take(&self, env: &Env<WithoutTls>) -> (Option<RoTxn<'static, WithoutTls>>, u64) {
        let mut state = self.shared.state.lock().expect("the idle cache lock is never poisoned");
        let generation = state.generation;
        if state.entries.is_empty() {
            return (None, generation);
        }
        let now = Instant::now();
        let current = env.info().last_txn_id;
        while let Some(entry) = state.entries.pop() {
            if entry.expires > now && entry.txn.id() == current {
                return (Some(entry.txn), generation);
            }
        }
        (None, generation)
    }

    pub fn put(&self, txn: RoTxn<'static, WithoutTls>, generation: u64) {
        let mut state = self.shared.state.lock().expect("the idle cache lock is never poisoned");
        if state.stopping || generation != state.generation || state.entries.len() >= super::MAX_IDLE {
            return;
        }
        let expires = Instant::now() + RETAIN_FOR;
        state.entries.push(Entry { txn, expires });
        if state.sweep.is_none() {
            state.sweep = Some(expires);
            self.shared.changed.notify_one();
        }
    }

    pub fn clear(&self) {
        let mut state = self.shared.state.lock().expect("the idle cache lock is never poisoned");
        state.generation = state.generation.wrapping_add(1);
        state.entries.clear();
    }

    pub fn close(&mut self) {
        let Some(worker) = self.worker.take() else { return };
        {
            let mut state = self.shared.state.lock().expect("the idle cache lock is never poisoned");
            state.stopping = true;
            state.entries.clear();
            self.shared.changed.notify_one();
        }
        worker.join().expect("the idle cache worker does not panic");
    }

    /// At process exit other threads may already be gone. Never join one or block on a lock it held.
    pub fn stop_at_exit(&mut self, deadline: Instant) {
        if self.worker.take().is_none() {
            return;
        }
        if let Some(mut state) = super::sync::lock_before(&self.shared.state, deadline) {
            state.stopping = true;
            state.entries.clear();
            self.shared.changed.notify_one();
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.shared.state.lock().unwrap().entries.len()
    }
}

impl Drop for IdleReads {
    fn drop(&mut self) {
        self.close();
    }
}

fn expire(shared: &Shared) {
    let mut state = shared.state.lock().expect("the idle cache lock is never poisoned");
    loop {
        if state.stopping {
            return;
        }
        match state.sweep {
            None => state = shared.changed.wait(state).expect("the idle cache lock is never poisoned"),
            Some(deadline) => {
                let now = Instant::now();
                if deadline > now {
                    state = shared
                        .changed
                        .wait_timeout(state, deadline - now)
                        .expect("the idle cache lock is never poisoned")
                        .0;
                } else {
                    state.entries.retain(|entry| entry.expires > now);
                    state.sweep = state.entries.iter().map(|entry| entry.expires).min();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_detaches_without_waiting_for_another_threads_cache_lock() {
        let mut idle = IdleReads::new().unwrap();
        let shared = Arc::clone(&idle.shared);
        let weak = Arc::downgrade(&shared);
        let (locked, wait_locked) = std::sync::mpsc::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let mut state = shared.state.lock().unwrap();
            locked.send(()).unwrap();
            wait_release.recv_timeout(Duration::from_secs(3)).unwrap();
            // Let the detached worker end normally after simulating a thread that was gone at exit.
            state.stopping = true;
            shared.changed.notify_one();
        });
        wait_locked.recv_timeout(Duration::from_secs(3)).unwrap();
        idle.stop_at_exit(Instant::now());
        drop(idle);
        release.send(()).unwrap();
        owner.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while weak.upgrade().is_some() {
            assert!(Instant::now() < deadline, "the detached cache worker kept its state alive");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
