//! Sans-I/O `WatchScheduler` with zero OS `Instant` calls and zero background threads!
//!
//! Driven deterministically by the host clock (`@apackdev/clock` in Electron/Node or `SimClock` in DST):
//! - Adaptive per-watch debounce gap: `max(min_gap_ms, adaptive_multiplier * last_cost_ms)` (defaults: `16ms`, `4x`)
//! - Cooperative `8ms` turn slice (`DEFAULT_TURN_SLICE_MS`) so UI frames are never starved
//! - 128-bit binary result dedup hash (`hash_result_128`, SHA-256 truncated to 16 bytes) for declarative watches
//! - Deterministic `caught_up(soon)`, `watches_settled()`, and `stop_watches()` lifecycle

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::read_set::WatchId;

/// Default minimum gap between successive runs of the same watch (`16ms` ≈ 1 frame at 60 Hz).
pub const DEFAULT_MIN_GAP_MS: u64 = 16;
/// Default adaptive multiplier (`4 * last_cost_ms`, capping single-watch CPU share to <= 20%).
pub const DEFAULT_ADAPTIVE_MULTIPLIER: u64 = 4;
/// Default cooperative turn slice budget (`8ms`).
pub const DEFAULT_TURN_SLICE_MS: u64 = 8;

/// Watch delivery semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    /// Declarative query watch: compares 128-bit binary result hash and skips delivery when identical!
    Declarative,
    /// Imperative callback watch: always delivers when re-executed after invalidation.
    Imperative,
}

/// Per-watch scheduling policy knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchPolicy {
    pub mode: WatchMode,
    pub min_gap_ms: u64,
    pub adaptive_multiplier: u64,
}

impl Default for WatchPolicy {
    fn default() -> Self {
        Self {
            mode: WatchMode::Declarative,
            min_gap_ms: DEFAULT_MIN_GAP_MS,
            adaptive_multiplier: DEFAULT_ADAPTIVE_MULTIPLIER,
        }
    }
}

/// Deterministic 128-bit binary result hash (`SHA-256[0..16]`) used to deduplicate declarative watch emissions.
pub fn hash_result_128(payload: &[u8]) -> [u8; 16] {
    let digest = Sha256::digest(payload);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

#[derive(Debug, Clone)]
struct WatchSlot {
    policy: WatchPolicy,
    dirty: bool,
    running: bool,
    dirty_since_ms: Option<u64>,
    last_run_ms: Option<u64>,
    last_cost_ms: u64,
    last_result_hash: Option<[u8; 16]>,
}

impl WatchSlot {
    fn gap_ms(&self) -> u64 {
        self.policy.min_gap_ms.max(self.last_cost_ms.saturating_mul(self.policy.adaptive_multiplier))
    }

    fn ready_at_ms(&self) -> u64 {
        match self.last_run_ms {
            None => 0,
            Some(last) => last.saturating_add(self.gap_ms()),
        }
    }
}

/// Result returned by `WatchScheduler::tick(now_ms)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickResult {
    /// Watches ready to execute at `now_ms`, ordered fairly by earliest `dirty_since_ms` then `WatchId`.
    pub due_ids: Vec<WatchId>,
    /// Delay in milliseconds until the next dirty (cooling-down) watch becomes ready, if any.
    pub next_wake_delay_ms: Option<u64>,
    /// `true` iff no watches are dirty or running (scheduler is completely idle).
    pub settled: bool,
}

/// Result returned by `WatchScheduler::next_due(now_ms, elapsed_turn_ms)` for cooperative turn-slicing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NextDue {
    /// Next due watch to execute in this turn, or `None` if turn slice is exhausted / no watch is due yet.
    pub watch_id: Option<WatchId>,
    /// Recommended host timer delay in milliseconds until the next turn or cooldown expiry (`Some(0)` if turn slice hit with remaining due watches!).
    pub next_wake_delay_ms: Option<u64>,
    /// `true` iff no watches are dirty or running.
    pub settled: bool,
}

/// Pure sans-I/O watch scheduler (zero OS time calls, zero background threads).
#[derive(Debug, Clone)]
pub struct WatchScheduler {
    turn_slice_ms: u64,
    watches: BTreeMap<WatchId, WatchSlot>,
    stopped: bool,
}

impl Default for WatchScheduler {
    fn default() -> Self {
        Self::new(DEFAULT_TURN_SLICE_MS)
    }
}

impl WatchScheduler {
    pub fn new(turn_slice_ms: u64) -> Self {
        Self { turn_slice_ms: turn_slice_ms.max(1), watches: BTreeMap::new(), stopped: false }
    }

    pub fn turn_slice_ms(&self) -> u64 {
        self.turn_slice_ms
    }

    pub fn active_count(&self) -> usize {
        self.watches.len()
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Registers a watch. If `initial_hash` is provided (from initial snapshot execution), it seeds dedup state
    /// and sets `last_run_ms = Some(now_ms)` with `dirty = false`; otherwise starts `dirty = true` for immediate run.
    pub fn register(
        &mut self,
        id: WatchId,
        policy: WatchPolicy,
        now_ms: u64,
        initial_cost_ms: u64,
        initial_hash: Option<[u8; 16]>,
    ) {
        if self.stopped {
            return;
        }
        let has_initial = initial_hash.is_some();
        self.watches.insert(
            id,
            WatchSlot {
                policy,
                dirty: !has_initial,
                running: false,
                dirty_since_ms: if has_initial { None } else { Some(now_ms) },
                last_run_ms: if has_initial { Some(now_ms) } else { None },
                last_cost_ms: initial_cost_ms,
                last_result_hash: initial_hash,
            },
        );
    }

    /// Removes a watch from scheduling.
    pub fn unregister(&mut self, id: WatchId) -> bool {
        self.watches.remove(&id).is_some()
    }

    /// Marks watches dirty at virtual time `now_ms`. Coalesces repeated invalidations without resetting `dirty_since_ms`!
    pub fn mark_dirty(&mut self, ids: &[WatchId], now_ms: u64) {
        if self.stopped {
            return;
        }
        for id in ids {
            if let Some(slot) = self.watches.get_mut(id)
                && !slot.dirty
            {
                slot.dirty = true;
                slot.dirty_since_ms = Some(now_ms);
            }
        }
    }

    /// Marks every registered watch dirty at `now_ms`.
    pub fn mark_all_dirty(&mut self, now_ms: u64) {
        if self.stopped {
            return;
        }
        for slot in self.watches.values_mut() {
            if !slot.dirty {
                slot.dirty = true;
                slot.dirty_since_ms = Some(now_ms);
            }
        }
    }

    /// Returns ALL currently due watches at `now_ms` along with the next wake delay for cooling-down watches.
    pub fn tick(&self, now_ms: u64) -> TickResult {
        if self.stopped {
            return TickResult { due_ids: Vec::new(), next_wake_delay_ms: None, settled: true };
        }
        let mut due: Vec<(u64, WatchId)> = Vec::new();
        let mut min_delay: Option<u64> = None;
        let mut any_pending = false;

        for (&id, slot) in &self.watches {
            if slot.running {
                any_pending = true;
                continue;
            }
            if !slot.dirty {
                continue;
            }
            any_pending = true;
            let ready_at = slot.ready_at_ms();
            if now_ms >= ready_at {
                due.push((slot.dirty_since_ms.unwrap_or(0), id));
            } else {
                let wait = ready_at.saturating_sub(now_ms);
                min_delay = Some(min_delay.map_or(wait, |cur| cur.min(wait)));
            }
        }
        due.sort_unstable();
        TickResult {
            due_ids: due.into_iter().map(|(_, id)| id).collect(),
            next_wake_delay_ms: min_delay,
            settled: !any_pending,
        }
    }

    /// Cooperative turn-sliced pump: picks at most one due watch if `elapsed_turn_ms < turn_slice_ms`
    /// (or if `elapsed_turn_ms == 0`, always allows at least 1 watch to make forward progress!),
    /// marks it `running = true`, and clears `dirty = false` before execution so concurrent commits
    /// during execution cleanly re-dirty it!
    pub fn next_due(&mut self, now_ms: u64, elapsed_turn_ms: u64) -> NextDue {
        if self.stopped {
            return NextDue { watch_id: None, next_wake_delay_ms: None, settled: true };
        }
        let slice_exhausted = elapsed_turn_ms >= self.turn_slice_ms;
        let mut best_due: Option<(u64, WatchId)> = None;
        let mut min_delay: Option<u64> = None;
        let mut any_pending = false;

        for (&id, slot) in &self.watches {
            if slot.running {
                any_pending = true;
                continue;
            }
            if !slot.dirty {
                continue;
            }
            any_pending = true;
            let ready_at = slot.ready_at_ms();
            if now_ms >= ready_at {
                let key = (slot.dirty_since_ms.unwrap_or(0), id);
                if best_due.is_none_or(|cur| key < cur) {
                    best_due = Some(key);
                }
            } else {
                let wait = ready_at.saturating_sub(now_ms);
                min_delay = Some(min_delay.map_or(wait, |cur| cur.min(wait)));
            }
        }

        if let Some((_, id)) = best_due {
            if slice_exhausted {
                // Turn slice budget reached: yield back to host event loop and resume on next tick!
                return NextDue { watch_id: None, next_wake_delay_ms: Some(0), settled: false };
            }
            if let Some(slot) = self.watches.get_mut(&id) {
                slot.running = true;
                slot.dirty = false;
                slot.dirty_since_ms = None;
            }
            return NextDue { watch_id: Some(id), next_wake_delay_ms: min_delay, settled: false };
        }

        NextDue { watch_id: None, next_wake_delay_ms: min_delay, settled: !any_pending }
    }

    /// Marks a watch as started when driven via `tick()` instead of `next_due()`.
    pub fn begin_run(&mut self, id: WatchId) -> bool {
        if self.stopped {
            return false;
        }
        if let Some(slot) = self.watches.get_mut(&id) {
            slot.running = true;
            slot.dirty = false;
            slot.dirty_since_ms = None;
            return true;
        }
        false
    }

    /// Records completion of watch `id` at `now_ms` taking `cost_ms`.
    /// Returns `should_deliver`:
    /// - For `WatchMode::Declarative`: `false` if `new_result_hash == previous_hash` (skipping redundant N-API/JS delivery!).
    /// - For `WatchMode::Imperative`: always `true` (unless watch was unregistered/stopped).
    pub fn finish_run(&mut self, id: WatchId, now_ms: u64, cost_ms: u64, new_result_hash: Option<[u8; 16]>) -> bool {
        if self.stopped {
            return false;
        }
        let Some(slot) = self.watches.get_mut(&id) else {
            return false;
        };
        slot.running = false;
        slot.last_run_ms = Some(now_ms);
        slot.last_cost_ms = cost_ms;
        match slot.policy.mode {
            WatchMode::Declarative => {
                if let Some(h) = new_result_hash {
                    let changed = slot.last_result_hash != Some(h);
                    slot.last_result_hash = Some(h);
                    changed
                } else {
                    true
                }
            }
            WatchMode::Imperative => {
                if let Some(h) = new_result_hash {
                    slot.last_result_hash = Some(h);
                }
                true
            }
        }
    }

    /// Computes the next wake delay in milliseconds at `now_ms` (used by host to arm its single timer).
    pub fn next_wake_delay_ms(&self, now_ms: u64) -> Option<u64> {
        if self.stopped {
            return None;
        }
        let mut min_delay: Option<u64> = None;
        for slot in self.watches.values() {
            if slot.running || !slot.dirty {
                continue;
            }
            let wait = slot.ready_at_ms().saturating_sub(now_ms);
            min_delay = Some(min_delay.map_or(wait, |cur| cur.min(wait)));
        }
        min_delay
    }

    /// `caught_up(soon: false)`: true iff NO watch is dirty or running.
    /// `caught_up(soon: true)`: true iff NO watch is running and NO watch is immediately due at `now_ms`
    /// (dirty watches may only be waiting on their adaptive cooldown gap).
    pub fn caught_up(&self, now_ms: u64, soon: bool) -> bool {
        if self.stopped {
            return true;
        }
        for slot in self.watches.values() {
            if slot.running {
                return false;
            }
            if slot.dirty && (!soon || now_ms >= slot.ready_at_ms()) {
                return false;
            }
        }
        true
    }

    /// Returns `true` iff all active watches have settled (zero dirty and zero running).
    pub fn watches_settled(&self) -> bool {
        if self.stopped {
            return true;
        }
        self.watches.values().all(|s| !s.dirty && !s.running)
    }

    /// Stops all watches cleanly (used during database close / test teardown).
    pub fn stop_watches(&mut self) {
        self.stopped = true;
        self.watches.clear();
    }
}
