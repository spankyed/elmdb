import type { QueryAnswer, QuerySpec, WatchPolicyOptions } from 'elmdb';
import type { EarsDatabase } from './database.ts';
import { EarsError, toEarsError } from './errors.ts';

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
  /** Adaptive backoff multiplier applied to rerun duration (default `2`). */
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

interface WatchEntry {
  readonly id: number;
  readonly spec: QuerySpec;
  readonly deliver: (answer: QueryAnswer) => void;
  readonly onError: ((error: EarsError) => void) | undefined;
  active: boolean;
}

/**
 * Instance-scoped sans-I/O watch pump.
 *
 * All invalidation tracking, coalescing, adaptive throttling, and turn-slicing state lives inside
 * Rust's `LiveQueryHub` on `EarsDatabase`; this pump simply drives `nextDueWatch` / `rerunWatch`
 * using a single injectable `EarsClock` timer (zero hidden Rust background timer threads).
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
    };
    this.#entries.set(reg.watchId, entry);

    if (options?.immediate !== false) {
      try {
        deliver(reg.answer);
      } catch (error) {
        this.#reportError(entry, toEarsError(error));
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

  #scheduleNext(nowMs: number): void {
    if (this.#stopped || this.#entries.size === 0) return;
    let tick;
    try {
      tick = this.#db.tickWatches(nowMs);
    } catch {
      return;
    }
    if (tick.settled && tick.dueIds.length === 0 && tick.nextWakeDelayMs === null) {
      this.#clearTimer();
      return;
    }
    const delayMs = tick.dueIds.length > 0 ? 0 : Math.max(0, tick.nextWakeDelayMs ?? 0);
    this.#clearTimer();
    this.#timer = this.#clock.setTimeout(() => {
      this.#timer = undefined;
      if (this.#stopped) return;
      this.#drain(this.#clock.now(), true);
    }, delayMs);
  }

  #drain(startNowMs: number, scheduleTail: boolean): number {
    if (this.#pumping || this.#stopped) return 0;
    this.#pumping = true;
    this.#rescheduleAfterPump = false;
    let ran = 0;
    let nextWakeDelayMs: number | undefined;
    try {
      const turnStart = startNowMs;
      // Guard against pathological self-triggering loops within a single synchronous drain.
      const maxIterations = Math.max(64, this.#entries.size * 8);
      for (let i = 0; i < maxIterations; i++) {
        if (this.#stopped || this.#entries.size === 0) break;
        const nowMs = scheduleTail ? this.#clock.now() : startNowMs;
        const elapsedTurnMs = Math.max(0, nowMs - turnStart);
        const due = this.#db.nextDueWatch(nowMs, elapsedTurnMs);
        if (due.watchId === undefined) {
          nextWakeDelayMs = due.nextWakeDelayMs;
          break;
        }
        const entry = this.#entries.get(due.watchId);
        if (!entry || !entry.active) {
          try {
            this.#db.unregisterWatch(due.watchId);
          } catch {
            // Already gone.
          }
          continue;
        }
        const runStart = this.#clock.now();
        try {
          const rerun = this.#db.rerunWatch(entry.id, entry.spec, nowMs, Math.max(1, this.#clock.now() - runStart));
          ran++;
          if (entry.active && rerun.changed) {
            try {
              entry.deliver(rerun.answer);
            } catch (callbackErr) {
              this.#reportError(entry, toEarsError(callbackErr));
            }
          }
        } catch (rerunErr) {
          this.#reportError(entry, toEarsError(rerunErr));
        }
      }
    } finally {
      this.#pumping = false;
    }

    if (scheduleTail && !this.#stopped && this.#entries.size > 0) {
      if (this.#rescheduleAfterPump) {
        this.#scheduleNext(this.#clock.now());
      } else if (nextWakeDelayMs !== undefined) {
        this.#clearTimer();
        this.#timer = this.#clock.setTimeout(() => {
          this.#timer = undefined;
          if (!this.#stopped) this.#drain(this.#clock.now(), true);
        }, Math.max(0, nextWakeDelayMs));
      } else {
        this.#scheduleNext(this.#clock.now());
      }
    }
    return ran;
  }

  #reportError(entry: WatchEntry, error: EarsError): void {
    const handler = entry.onError ?? this.#onWatchError;
    if (handler) {
      handler(error);
    }
  }
}
