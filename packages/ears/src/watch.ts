import type { QueryAnswer, QuerySpec, WatchPolicyOptions } from 'elmdb';
import type { EarsDatabase } from './database.ts';
import { EarsError, toEarsError, type EarsErrorCode } from './errors.ts';

/**
 * Injectable clock and timer interface for deterministic sans-I/O watch scheduling.
 * Works identically in Node.js, Electron renderers, browsers, and deterministic unit tests.
 */
export interface EarsClock {
  now(): number;
  setTimeout(callback: () => void, delayMs: number): unknown;
  clearTimeout(handle: unknown): void;
}

/** Delivery mode for live query watches. */
export type WatchMode = 'declarative' | 'imperative';

/** Options configuring a live query watch. */
export interface WatchOptions {
  /** `'declarative'` (default, 128-bit answer dedup) or `'imperative'` (always deliver on invalidation). */
  readonly mode?: WatchMode;
  /** Minimum delay between consecutive reruns of this watch in milliseconds. */
  readonly minGapMs?: number;
  /** Adaptive backoff multiplier applied to rerun duration (default `4`). */
  readonly adaptiveMultiplier?: number;
  /** Deliver the initial query snapshot immediately upon registration (default `true`). */
  readonly immediate?: boolean;
  /** Optional error sink invoked if a watch rerun or callback throws. */
  readonly onError?: (error: EarsError) => void;
}

/** Handle returned when subscribing to a live query watch. */
export interface WatchHandle {
  readonly id: number;
  readonly active: boolean;
  stop(): void;
}

/** Engine-level controller for driving and inspecting sans-I/O live watches. */
export interface WatchController {
  /** Number of currently active watches registered through this engine. */
  readonly activeCount: number;
  /** Polls persisted cross-process commits, dirties affected watches, and schedules a pump pass. */
  catchUpCrossProcess(): number[];
  /** Synchronously drains all watches currently due at `nowMs` (defaults to `clock.now()`). */
  flushSync(nowMs?: number): number;
  /** True when no watches are dirty or currently running. */
  settled(): boolean;
  /** True when all watches are caught up (or, with `soon: true`, have no immediately due work). */
  caughtUp(soon?: boolean): boolean;
  /** Unregisters and stops every active watch on this engine. */
  stopAll(): void;
}

/** Creates the default system clock backed by `Date.now` and global timers (zero Node-specific APIs). */
export function createSystemClock(): EarsClock {
  return {
    now: () => Date.now(),
    setTimeout: (callback, delayMs) => globalThis.setTimeout(callback, delayMs),
    clearTimeout: (handle) => {
      globalThis.clearTimeout(handle as ReturnType<typeof globalThis.setTimeout>);
    },
  };
}

/**
 * Error codes indicating a watch's compiled query can no longer succeed on the current schema/handle.
 * Such watches are automatically unregistered on rerun failure instead of spinning in a 0ms livelock.
 */
const PERMANENT_WATCH_ERROR_CODES: Readonly<Partial<Record<EarsErrorCode, true>>> = Object.freeze({
  UNKNOWN_TYPE: true,
  TYPE_ARCHIVED: true,
  UNKNOWN_FIELD: true,
  UNKNOWN_RELATION: true,
  UNKNOWN_INDEX: true,
  INVALID_ARGUMENT: true,
  CLOSED: true,
});

const INITIAL_TRANSIENT_BACKOFF_MS = 50;
const MAX_TRANSIENT_BACKOFF_MS = 30_000;

interface WatchEntry {
  readonly id: number;
  readonly spec: QuerySpec;
  readonly deliver: (answer: QueryAnswer) => void;
  readonly onError: ((error: EarsError) => void) | undefined;
  active: boolean;
  /** Measured wall-clock duration (ms) of the previous rerun, fed into Rust's adaptive backoff. */
  lastCostMs: number;
  /** Backoff state for transient rerun failures so a broken watch never spins at 0ms. */
  backoffMs: number;
  backoffUntilMs: number;
}

/**
 * Instance-scoped sans-I/O watch pump.
 *
 * All invalidation tracking, coalescing, adaptive throttling, and turn-slicing state lives inside
 * Rust's `LiveQueryHub` on `EarsDatabase`; this pump drives `nextDueWatch` / `rerunWatch` using
 * a single injectable `EarsClock` timer (zero hidden Rust background timer threads).
 */
export class WatchPump implements WatchController {
  readonly #db: EarsDatabase;
  readonly #clock: EarsClock;
  readonly #onWatchError: ((error: EarsError) => void) | undefined;
  readonly #entries = new Map<number, WatchEntry>();
  #timer: unknown = undefined;
  #pumping = false;
  #rescheduleAfterPump = false;
  #stopped = false;

  constructor(db: EarsDatabase, clock: EarsClock, onWatchError?: (error: EarsError) => void) {
    this.#db = db;
    this.#clock = clock;
    this.#onWatchError = onWatchError;
  }

  get activeCount(): number {
    return this.#entries.size;
  }

  register(
    spec: QuerySpec,
    deliver: (answer: QueryAnswer) => void,
    options?: WatchOptions,
  ): WatchHandle {
    if (this.#stopped) {
      throw new EarsError('CLOSED', 'Cannot register a watch after the engine has stopped');
    }
    const policy: WatchPolicyOptions = {};
    if (options?.mode !== undefined) policy.mode = options.mode;
    if (options?.minGapMs !== undefined) policy.minGapMs = options.minGapMs;
    if (options?.adaptiveMultiplier !== undefined) policy.adaptiveMultiplier = options.adaptiveMultiplier;

    const nowMs = this.#clock.now();
    let reg;
    try {
      reg = this.#db.registerWatch(spec, policy, nowMs);
    } catch (error) {
      throw toEarsError(error);
    }

    const entry: WatchEntry = {
      id: reg.watchId,
      spec,
      deliver,
      onError: options?.onError,
      active: true,
      lastCostMs: 1,
      backoffMs: 0,
      backoffUntilMs: 0,
    };
    this.#entries.set(reg.watchId, entry);

    if (options?.immediate !== false) {
      try {
        deliver(reg.answer);
      } catch (error) {
        try {
          this.#reportError(entry, toEarsError(error), false);
        } catch (handlerError) {
          // Never leak the registered watch if the caller's error handler rethrows before we return WatchHandle.
          this.#unregister(entry.id);
          throw handlerError;
        }
      }
    }

    // If a concurrent write landed during registration or inside the immediate callback, schedule a wake.
    if (entry.active && !this.#stopped) {
      try {
        if (!this.#db.watchesSettled()) {
          this.#scheduleNext(this.#clock.now());
        }
      } catch {
        // Ignore if database closed inside callback.
      }
    }

    const self = this;
    return {
      get id(): number {
        return entry.id;
      },
      get active(): boolean {
        return entry.active;
      },
      stop(): void {
        self.#unregister(entry.id);
      },
    };
  }

  notifyMutation(): void {
    if (this.#stopped || this.#entries.size === 0) return;
    if (this.#pumping) {
      this.#rescheduleAfterPump = true;
      return;
    }
    this.#scheduleNext(this.#clock.now());
  }

  catchUpCrossProcess(): number[] {
    if (this.#stopped) return [];
    let dirtied: number[];
    try {
      dirtied = this.#db.catchUpCrossProcess(this.#clock.now());
    } catch (error) {
      throw toEarsError(error);
    }
    if (dirtied.length > 0) this.notifyMutation();
    return dirtied;
  }

  flushSync(nowMs?: number): number {
    if (this.#stopped || this.#entries.size === 0) return 0;
    const startMs = nowMs ?? this.#clock.now();
    return this.#drain(startMs, false);
  }

  settled(): boolean {
    if (this.#stopped) return true;
    try {
      return this.#db.watchesSettled();
    } catch (error) {
      throw toEarsError(error);
    }
  }

  caughtUp(soon?: boolean): boolean {
    if (this.#stopped) return true;
    try {
      return this.#db.caughtUp(this.#clock.now(), soon ?? false);
    } catch (error) {
      throw toEarsError(error);
    }
  }

  stopAll(): void {
    this.#clearTimer();
    for (const entry of this.#entries.values()) {
      entry.active = false;
    }
    this.#entries.clear();
    try {
      this.#db.stopWatches();
    } catch {
      // Best-effort if the underlying database is already closed.
    }
  }

  markClosed(): void {
    this.#stopped = true;
    this.stopAll();
  }

  #unregister(watchId: number): void {
    const entry = this.#entries.get(watchId);
    if (!entry) return;
    entry.active = false;
    this.#entries.delete(watchId);
    try {
      this.#db.unregisterWatch(watchId);
    } catch {
      // Ignore if DB was already closed.
    }
    if (this.#entries.size === 0) this.#clearTimer();
  }

  #clearTimer(): void {
    if (this.#timer !== undefined) {
      this.#clock.clearTimeout(this.#timer);
      this.#timer = undefined;
    }
  }

  #armTimer(delayMs: number): void {
    this.#clearTimer();
    this.#timer = this.#clock.setTimeout(() => {
      this.#timer = undefined;
      if (this.#stopped) return;
      this.#drain(this.#clock.now(), true);
    }, Math.max(0, delayMs));
  }

  #scheduleNext(nowMs: number): void {
    if (this.#stopped || this.#entries.size === 0) {
      this.#clearTimer();
      return;
    }
    let tick;
    try {
      tick = this.#db.tickWatches(nowMs);
    } catch {
      this.#clearTimer();
      return;
    }
    if (tick.dueIds.length === 0 && tick.nextWakeDelayMs === null) {
      this.#clearTimer();
      return;
    }
    let delayMs: number | undefined;
    if (tick.dueIds.length > 0) {
      // Respect per-watch transient failure backoff so failing watches never spin at 0ms.
      let earliestReadyDelay = Infinity;
      for (const id of tick.dueIds) {
        const entry = this.#entries.get(id);
        const wait = entry && entry.backoffUntilMs > nowMs ? entry.backoffUntilMs - nowMs : 0;
        if (wait < earliestReadyDelay) earliestReadyDelay = wait;
      }
      delayMs = Number.isFinite(earliestReadyDelay) ? earliestReadyDelay : 0;
    } else if (tick.nextWakeDelayMs !== null) {
      delayMs = Math.max(0, tick.nextWakeDelayMs);
    }
    if (delayMs === undefined) {
      this.#clearTimer();
      return;
    }
    this.#armTimer(delayMs);
  }

  #drain(startNowMs: number, scheduleTail: boolean): number {
    if (this.#pumping || this.#stopped) return 0;
    this.#pumping = true;
    this.#rescheduleAfterPump = false;
    let ran = 0;
    let nextWakeDelayMs: number | undefined;
    let backoffDelayMs: number | undefined;
    const failedThisTurn = new Set<number>();

    try {
      const turnStart = startNowMs;
      const maxIterations = Math.max(64, this.#entries.size * 8);
      for (let i = 0; i < maxIterations; i++) {
        if (this.#stopped || this.#entries.size === 0) break;
        const nowMs = scheduleTail ? this.#clock.now() : startNowMs;
        const elapsedTurnMs = Math.max(0, nowMs - turnStart);
        let watchId: number | undefined;

        if (this.#hasBlockedEntries(nowMs, failedThisTurn)) {
          let tick;
          try {
            tick = this.#db.tickWatches(nowMs);
          } catch {
            break;
          }
          for (const candidate of tick.dueIds) {
            const candidateEntry = this.#entries.get(candidate);
            if (!candidateEntry || !candidateEntry.active) {
              watchId = candidate;
              break;
            }
            if (!failedThisTurn.has(candidate) && candidateEntry.backoffUntilMs <= nowMs) {
              watchId = candidate;
              break;
            }
          }
          if (watchId === undefined) {
            for (const candidate of tick.dueIds) {
              const candidateEntry = this.#entries.get(candidate);
              if (candidateEntry) {
                const wait = Math.max(INITIAL_TRANSIENT_BACKOFF_MS, candidateEntry.backoffUntilMs - nowMs);
                backoffDelayMs = backoffDelayMs === undefined ? wait : Math.min(backoffDelayMs, wait);
              }
            }
            if (tick.nextWakeDelayMs !== null) {
              nextWakeDelayMs = tick.nextWakeDelayMs;
            }
            break;
          }
        } else {
          let due;
          try {
            due = this.#db.nextDueWatch(nowMs, elapsedTurnMs);
          } catch {
            break;
          }
          if (due.watchId === undefined) {
            nextWakeDelayMs = due.nextWakeDelayMs;
            break;
          }
          watchId = due.watchId;
        }

        const entry = this.#entries.get(watchId);
        if (!entry || !entry.active) {
          try {
            this.#db.unregisterWatch(watchId);
          } catch {
            // Already gone.
          }
          continue;
        }

        const runStart = this.#clock.now();
        try {
          const rerun = this.#db.rerunWatch(entry.id, entry.spec, nowMs, entry.lastCostMs);
          entry.lastCostMs = Math.max(1, this.#clock.now() - runStart);
          entry.backoffMs = 0;
          entry.backoffUntilMs = 0;
          ran++;
          if (entry.active && rerun.changed) {
            try {
              entry.deliver(rerun.answer);
            } catch (callbackErr) {
              this.#reportError(entry, toEarsError(callbackErr), true);
            }
          }
        } catch (rerunErr) {
          const earsErr = toEarsError(rerunErr);
          failedThisTurn.add(entry.id);
          if (Object.prototype.hasOwnProperty.call(PERMANENT_WATCH_ERROR_CODES, earsErr.code)) {
            // Permanent schema/compile/closed error: stop the broken watch so it cannot livelock or starve others.
            this.#unregister(entry.id);
          } else {
            // Transient error: exponential backoff instead of 0ms spin.
            entry.backoffMs =
              entry.backoffMs === 0
                ? INITIAL_TRANSIENT_BACKOFF_MS
                : Math.min(MAX_TRANSIENT_BACKOFF_MS, entry.backoffMs * 2);
            entry.backoffUntilMs = nowMs + entry.backoffMs;
            backoffDelayMs =
              backoffDelayMs === undefined ? entry.backoffMs : Math.min(backoffDelayMs, entry.backoffMs);
          }
          this.#reportError(entry, earsErr, true);
          // Break if all remaining due watches are backed off so we don't pop the same watch again in this turn.
          if (this.#allDueBlocked(nowMs, failedThisTurn)) break;
        }
      }
    } finally {
      this.#pumping = false;
    }

    if (this.#stopped || this.#entries.size === 0) {
      this.#clearTimer();
      return ran;
    }

    if (backoffDelayMs !== undefined) {
      this.#armTimer(backoffDelayMs);
    } else if (this.#rescheduleAfterPump || scheduleTail) {
      if (nextWakeDelayMs !== undefined && !this.#rescheduleAfterPump) {
        this.#armTimer(nextWakeDelayMs);
      } else {
        this.#scheduleNext(this.#clock.now());
      }
    } else {
      // flushSync tail reconciliation: clear any stale timer if settled, or schedule remaining cooldown work.
      this.#scheduleNext(this.#clock.now());
    }
    return ran;
  }

  #hasBlockedEntries(nowMs: number, failedThisTurn: ReadonlySet<number>): boolean {
    if (failedThisTurn.size > 0) return true;
    for (const entry of this.#entries.values()) {
      if (entry.backoffUntilMs > nowMs) return true;
    }
    return false;
  }

  #allDueBlocked(nowMs: number, failedThisTurn: ReadonlySet<number>): boolean {
    try {
      const tick = this.#db.tickWatches(nowMs);
      if (tick.dueIds.length === 0) return true;
      return tick.dueIds.every((id) => {
        if (failedThisTurn.has(id)) return true;
        const entry = this.#entries.get(id);
        return entry !== undefined && entry.backoffUntilMs > nowMs;
      });
    } catch {
      return true;
    }
  }

  #reportError(entry: WatchEntry, error: EarsError, isolateHandler: boolean): void {
    const handler = entry.onError ?? this.#onWatchError;
    if (!handler) return;
    if (!isolateHandler) {
      handler(error);
      return;
    }
    try {
      handler(error);
    } catch {
      // Isolate throwing error handlers during pump drain so healthy watches still get scheduled.
    }
  }
}
