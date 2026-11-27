import type { QueryAnswer, QuerySpec, WatchPolicyOptions } from 'elmdb';
import type { EarsDatabase } from './database.ts';
import { EarsError, toEarsError, type EarsErrorCode } from './errors.ts';
import type { EarsOperationObserver } from './observer.ts';

/**
 * Clock interface matching `@apackdev/clock` / `@apackdev/state` (`now()` + `after(ms, run) => cancel`).
 */
export interface AfterClock {
  now(): number;
  after(ms: number, run: () => void): () => void;
}

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
  /**
   * Registers a one-shot listener invoked as soon as all watches are settled (`settled() === true`).
   * If already settled, invokes `listener` immediately and returns a no-op unsubscribe function.
   */
  onSettled(listener: () => void): () => void;
  /** Unregisters and stops every active watch on this engine. */
  stopAll(): void;
}

/** Creates the default system clock backed by `Date.now` and global timers (zero Node-specific APIs). */
export function createSystemClock(): EarsClock & AfterClock {
  return {
    now: () => Date.now(),
    after: (ms, run) => {
      const handle = globalThis.setTimeout(run, ms);
      return () => {
        globalThis.clearTimeout(handle);
      };
    },
    setTimeout: (callback, delayMs) => globalThis.setTimeout(callback, delayMs),
    clearTimeout: (handle) => {
      globalThis.clearTimeout(handle as ReturnType<typeof globalThis.setTimeout>);
    },
  };
}

/**
 * Compile-time exhaustive retry policy for every canonical `EarsErrorCode`.
 * - `'fatal_handle'`: the database handle itself is permanently unusable (`PANICKED`, `POISONED`, `CORRUPTED`,
 *   `DURABILITY_FAILED`); fail-stop the entire watch pump, deactivate all watches, notify all error handlers,
 *   and settle cleanly without calling dead native methods.
 * - `'permanent_watch'`: this watch's query can no longer succeed on the current schema/handle; unregister it.
 * - `'transient'`: apply bounded exponential backoff so failing watches never livelock at 0ms.
 */
type WatchErrorPolicy = 'transient' | 'permanent_watch' | 'fatal_handle';

const WATCH_ERROR_POLICY: Readonly<Record<EarsErrorCode, WatchErrorPolicy>> = Object.freeze({
  INVALID_ARGUMENT: 'permanent_watch',
  INVALID_WIRE_FRAME: 'permanent_watch',
  INVALID_SCHEMA: 'permanent_watch',
  SCHEMA_CHANGED: 'transient',
  UNKNOWN_TYPE: 'permanent_watch',
  TYPE_ARCHIVED: 'permanent_watch',
  UNKNOWN_FIELD: 'permanent_watch',
  UNKNOWN_RELATION: 'permanent_watch',
  UNKNOWN_INDEX: 'permanent_watch',
  ENTITY_NOT_FOUND: 'transient',
  ENTITY_ALREADY_EXISTS: 'transient',
  ENTITY_TRASHED: 'transient',
  ENTITY_NOT_TRASHED: 'transient',
  RELATION_NOT_FOUND: 'transient',
  RELATION_ALREADY_EXISTS: 'transient',
  VALIDATION_FAILED: 'permanent_watch',
  UNIQUE_VIOLATION: 'transient',
  CARDINALITY_VIOLATION: 'transient',
  CYCLE_DETECTED: 'transient',
  REFERENCED: 'transient',
  ENDPOINT_MISMATCH: 'permanent_watch',
  HOOK_ABORTED: 'transient',
  HOOK_CYCLE_LIMIT: 'transient',
  ASYNC_CALLBACK_FORBIDDEN: 'permanent_watch',
  INDEX_BUILD_FAILED: 'transient',
  INDEX_NOT_READY: 'transient',
  RESOURCE_LIMIT: 'transient',
  CANCELLED: 'transient',
  STORAGE_FULL: 'transient',
  CORRUPTED: 'fatal_handle',
  DURABILITY_FAILED: 'fatal_handle',
  POISONED: 'fatal_handle',
  IO: 'transient',
  INTERNAL: 'transient',
  CLOSED: 'permanent_watch',
  READ_ONLY: 'permanent_watch',
  NO_DATABASE: 'permanent_watch',
  DATABASE_LOCKED: 'transient',
  ALREADY_EXISTS: 'permanent_watch',
  PANICKED: 'fatal_handle',
  ALREADY_LOADED: 'permanent_watch',
});

const INITIAL_TRANSIENT_BACKOFF_MS = 50;
const MAX_TRANSIENT_BACKOFF_MS = 30_000;
const DEFAULT_TURN_SLICE_MS = 8;

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
  readonly #clock: EarsClock | AfterClock;
  readonly #onWatchError: ((error: EarsError) => void) | undefined;
  readonly #observer: EarsOperationObserver | undefined;
  readonly #turnSliceMs: number;
  readonly #entries = new Map<number, WatchEntry>();
  readonly #settledListeners = new Set<{ readonly run: () => void }>();
  #cancelTimer: (() => void) | undefined = undefined;
  #pumping = false;
  #rescheduleAfterPump = false;
  #stopped = false;

  constructor(
    db: EarsDatabase,
    clock: EarsClock | AfterClock,
    onWatchError?: (error: EarsError) => void,
    observer?: EarsOperationObserver,
    turnSliceMs = DEFAULT_TURN_SLICE_MS,
  ) {
    if (!Number.isSafeInteger(turnSliceMs) || turnSliceMs < 1) {
      throw new EarsError(
        'INVALID_ARGUMENT',
        `watchTurnSliceMs must be a positive safe integer, got ${String(turnSliceMs)}`,
      );
    }
    this.#db = db;
    this.#clock = clock;
    this.#onWatchError = onWatchError;
    this.#observer = observer;
    this.#turnSliceMs = turnSliceMs;
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
    const finish = this.#observer?.start({ kind: 'read', ops: 1, watch: true, inUnit: false });
    let reg;
    try {
      reg = this.#db.registerWatch(spec, policy, nowMs);
    } catch (error) {
      const earsErr = toEarsError(error);
      finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
      throw earsErr;
    }
    finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });

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

  onSettled(listener: () => void): () => void {
    if (!this.#pumping && this.settled()) {
      listener();
      return () => {};
    }
    const token = { run: listener };
    this.#settledListeners.add(token);
    return () => {
      this.#settledListeners.delete(token);
    };
  }

  stopAll(): void {
    this.#clearTimer();
    const watchIds = [...this.#entries.keys()];
    for (const watchId of watchIds) {
      this.#unregister(watchId);
    }
    if (!this.#pumping) this.#notifySettledIfReady();
  }

  markClosed(): void {
    this.#stopped = true;
    this.stopAll();
    try {
      this.#db.stopWatches();
    } catch {
      // Best-effort if the underlying database is already closed.
    }
    if (!this.#pumping) this.#notifySettledIfReady();
  }

  #notifySettledIfReady(): void {
    if (this.#settledListeners.size === 0) return;
    let isSettled: boolean;
    try {
      isSettled = this.settled();
    } catch {
      return;
    }
    if (!isSettled) return;
    const listeners = [...this.#settledListeners];
    this.#settledListeners.clear();
    for (const token of listeners) {
      try {
        token.run();
      } catch {
        // Isolate listener exceptions so remaining settled listeners still run.
      }
    }
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
    if (!this.#pumping) this.#notifySettledIfReady();
  }

  #clearTimer(): void {
    if (this.#cancelTimer !== undefined) {
      const cancel = this.#cancelTimer;
      this.#cancelTimer = undefined;
      cancel();
    }
  }

  #armTimer(delayMs: number): void {
    this.#clearTimer();
    const delay = Math.max(0, delayMs);
    const run = (): void => {
      this.#cancelTimer = undefined;
      if (this.#stopped) return;
      this.#drain(this.#clock.now(), true);
    };
    if ('after' in this.#clock && typeof this.#clock.after === 'function') {
      this.#cancelTimer = this.#clock.after(delay, run);
    } else {
      const clock = this.#clock as EarsClock;
      const handle = clock.setTimeout(run, delay);
      this.#cancelTimer = () => {
        clock.clearTimeout(handle);
      };
    }
  }

  #computeNextDelay(nowMs: number): number | undefined {
    let tick;
    try {
      tick = this.#db.tickWatches(nowMs);
    } catch {
      return undefined;
    }
    if (tick.dueIds.length === 0 && tick.nextWakeDelayMs === null) {
      return undefined;
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
      if (Number.isFinite(earliestReadyDelay)) {
        delayMs = earliestReadyDelay;
      }
    }
    // Always consider cooldown watches (`nextWakeDelayMs`) alongside backed-off due watches
    // so a failing watch in exponential backoff can never stall healthy watches up to 30s!
    if (tick.nextWakeDelayMs !== null) {
      const cooldown = Math.max(0, tick.nextWakeDelayMs);
      delayMs = delayMs === undefined ? cooldown : Math.min(delayMs, cooldown);
    }
    return delayMs;
  }

  #scheduleNext(nowMs: number): void {
    if (this.#stopped || this.#entries.size === 0) {
      this.#clearTimer();
      return;
    }
    const delayMs = this.#computeNextDelay(nowMs);
    if (delayMs === undefined) {
      this.#clearTimer();
      return;
    }
    this.#armTimer(delayMs);
  }

  #failStopHandle(error: EarsError, firstEntry?: WatchEntry): void {
    this.#stopped = true;
    this.#clearTimer();
    const remaining = [...this.#entries.values()];
    this.#entries.clear();
    if (firstEntry !== undefined) {
      firstEntry.active = false;
    }
    for (const entry of remaining) {
      entry.active = false;
    }
    if (firstEntry !== undefined) {
      this.#reportError(firstEntry, error, true);
    }
    for (const entry of remaining) {
      if (firstEntry === undefined || entry.id !== firstEntry.id) {
        this.#reportError(entry, error, true);
      }
    }
  }

  #handlePumpReadError(err: unknown): void {
    const earsErr = toEarsError(err);
    if (WATCH_ERROR_POLICY[earsErr.code] === 'fatal_handle' || earsErr.code === 'CLOSED') {
      this.#failStopHandle(earsErr);
    }
  }

  #drain(startNowMs: number, scheduleTail: boolean): number {
    if (this.#pumping || this.#stopped) return 0;
    this.#pumping = true;
    this.#rescheduleAfterPump = false;
    let ran = 0;
    let nextWakeDelayMs: number | undefined;
    let sawBackoff = false;
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
          // Enforce the 8ms cooperative turn slice even when another watch is backed off!
          if (scheduleTail && ran > 0 && elapsedTurnMs >= this.#turnSliceMs) {
            nextWakeDelayMs = 0;
            break;
          }
          let tick;
          try {
            tick = this.#db.tickWatches(nowMs);
          } catch (tickErr) {
            this.#handlePumpReadError(tickErr);
            break;
          }
          let dbFailed = false;
          for (const candidate of tick.dueIds) {
            const candidateEntry = this.#entries.get(candidate);
            if (!candidateEntry || !candidateEntry.active) {
              watchId = candidate;
              break;
            }
            if (!failedThisTurn.has(candidate) && candidateEntry.backoffUntilMs <= nowMs) {
              // Mark running & clear dirty atomically (matching nextDueWatch) so rerunWatch
              // does not leave the watch permanently dirty after completing!
              let began: boolean;
              try {
                began = this.#db.beginWatchRun(candidate);
              } catch (beginErr) {
                this.#handlePumpReadError(beginErr);
                dbFailed = true;
                break;
              }
              if (!began) {
                continue;
              }
              watchId = candidate;
              break;
            }
          }
          if (dbFailed) {
            break;
          }
          if (watchId === undefined) {
            sawBackoff = true;
            if (tick.nextWakeDelayMs !== null) {
              nextWakeDelayMs =
                nextWakeDelayMs === undefined
                  ? tick.nextWakeDelayMs
                  : Math.min(nextWakeDelayMs, tick.nextWakeDelayMs);
            }
            break;
          }
        } else {
          let due;
          try {
            due = this.#db.nextDueWatch(nowMs, elapsedTurnMs);
          } catch (dueErr) {
            this.#handlePumpReadError(dueErr);
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
        const finishRerun = this.#observer?.start({ kind: 'read', ops: 1, watch: true, inUnit: false });
        if (!entry.active) {
          finishRerun?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
          continue;
        }
        let rerun;
        try {
          rerun = this.#db.rerunWatch(entry.id, entry.spec, nowMs, entry.lastCostMs);
        } catch (rerunErr) {
          const earsErr = toEarsError(rerunErr);
          finishRerun?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
          failedThisTurn.add(entry.id);
          const policy = WATCH_ERROR_POLICY[earsErr.code];
          if (policy === 'fatal_handle') {
            this.#failStopHandle(earsErr, entry);
            break;
          }
          if (policy === 'permanent_watch') {
            // Permanent schema/compile/closed error: stop this broken watch so it cannot livelock or starve others.
            this.#unregister(entry.id);
          } else {
            // Ensure Rust WatchScheduler clears `running = false` and requeues `dirty = true` even if
            // rerunWatch threw before entering Rust's snapshot executor (preventing permanent wedges).
            try {
              this.#db.abortWatchRun(entry.id, true, nowMs);
            } catch {
              // Best-effort if DB closed.
            }
            // Transient error: exponential backoff instead of 0ms spin.
            entry.backoffMs =
              entry.backoffMs === 0
                ? INITIAL_TRANSIENT_BACKOFF_MS
                : Math.min(MAX_TRANSIENT_BACKOFF_MS, entry.backoffMs * 2);
            entry.backoffUntilMs = nowMs + entry.backoffMs;
            sawBackoff = true;
          }
          this.#reportError(entry, earsErr, true);
          // Break if all remaining due watches are backed off so we don't pop the same watch again in this turn.
          if (this.#allDueBlocked(nowMs, failedThisTurn)) break;
          continue;
        }
        try {
          finishRerun?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
        } catch (observerErr) {
          this.#reportError(entry, toEarsError(observerErr), true);
        }
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
      }
    } finally {
      this.#pumping = false;
    }

    if (this.#stopped || this.#entries.size === 0) {
      this.#clearTimer();
      this.#notifySettledIfReady();
      return ran;
    }

    // Recompute live next wake delay from the Rust scheduler + current active backoff deadlines
    // (avoiding stale orphan timers if a failing watch stopped itself inside `onError`).
    const candidates: number[] = [];
    if (nextWakeDelayMs !== undefined && !sawBackoff) {
      candidates.push(Math.max(0, nextWakeDelayMs));
    }
    if (this.#rescheduleAfterPump || !scheduleTail || sawBackoff || candidates.length === 0) {
      const scheduledDelay = this.#computeNextDelay(this.#clock.now());
      if (scheduledDelay !== undefined) {
        candidates.push(scheduledDelay);
      }
    }
    if (candidates.length === 0) {
      this.#clearTimer();
    } else {
      this.#armTimer(Math.min(...candidates));
    }
    this.#notifySettledIfReady();
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
