import { WatchCauses } from './observation.ts';
// Live reads: `watch` runs a read with every database read it makes tracked under a token of its own, which elmdb files
// in its interval index (`elmdb/docs/architecture/engine.md`, Change sets and watch), and runs it again after each
// commit that names the token, handing the listener the result when it changed. A result equal to the last delivered is
// not delivered. A read that consults anything but the database isn't tracked, and goes stale when that changes: a
// watched read is a pure read of the engine.
//
// How often a watch runs again is this scheduler's, whatever writes, and no view or writer opts in (G14 of elmdb's
// goal-elmdb-next.md). Re-runs wait until the writing code has returned (a microtask), so a burst of commits in one
// turn costs one. A watch has at most one run pending: commits arriving meanwhile only mark it dirty, so the run that
// follows reads the latest state and the states between are skipped. A watch that ran recently waits a gap
// proportional to what that run cost, and at least a frame, before it runs again; one idle when a commit lands runs at
// once. A flush spends at most a slice of time on runs, and the watches left over run first in the next one; a watch
// whose last run would overrun what is left waits for that, so an expensive watch never holds a cheap one back.
import type { Commit, Connection } from './database.ts';
import { callSite } from './calls.ts';

/** What a watch delivers to: each changed result, and what a read threw, with the commit it reflects */
export interface WatchObserver<T> {
  /** A result that differs from the last one delivered (the first, always) */
  next(value: T, commit: number): void;
  /** What the read threw; it stays watched, and runs again after a commit that touches what it read before throwing */
  error?(error: unknown, commit: number): void;
}

/** Diagnostic causes of a scheduled read, bounded independently of the number of coalesced commits. */
export interface WatchReadContext {
  readonly sequences: readonly number[];
  /** Commit causes omitted because the bound or the diagnostic policy failed. */
  readonly dropped: number;
}

/** How a watch is named where its cost is reported */
export interface WatchOptions {
  /** Opt-in diagnostic metadata only; never changes when a watch runs. Evaluated at invalidation. */
  recordCauses?: () => boolean;
  /** What the watch is, in the call count and in a warning about its cost; where it was opened when not given */
  label?: string;
}

/** An open watch */
export interface Watching {
  /** Ends the watch: nothing is delivered after it */
  stop(): void;
}

/** The clock a scheduler reads and the timers it sets: the process's, or a test's */
export interface WatchClock {
  now(): number;
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

/**
 * The scheduler's constants. A watch's gap after a run is `delayFactor` times that run's time, and at least
 * `minimumGapMs`; a flush starts no run past `flushSliceMs` of runs. Set by the stress reading
 * (`bench/watch-stress.ts`; elmdb's docs/evidence/next-phase-3.md)
 */
export interface WatchSchedule {
  delayFactor: number;
  minimumGapMs: number;
  flushSliceMs: number;
}

/** The constants the engine's watches run under: a gap of four times a run and at least a frame, an 8 ms slice */
export const WATCH_SCHEDULE: WatchSchedule = Object.freeze({ delayFactor: 4, minimumGapMs: 16, flushSliceMs: 8 });

/** How many flushes in a row a watch's run may exceed the slice before a warning names it */
const OVER_BUDGET_WARNING = 3;

/** A watch as the registry holds it */
interface Watch {
  token: number;
  label: string;
  read: (context: WatchReadContext | undefined) => unknown;
  causes?: WatchCauses;
  observer: WatchObserver<unknown>;
  /** What was last delivered: a value, an error, or nothing yet */
  last: { ok: true; value: unknown } | { ok: false; error: unknown } | undefined;
  stopped: boolean;
  /** When its last run ended and what it cost, so when it may run again */
  ranAt: number;
  costMs: number;
  /** Flushes in a row whose run of it took more than the slice, and whether that has been warned about */
  overBudget: number;
  warned: boolean;
}

export interface Watches {
  watch<T>(read: (context: WatchReadContext | undefined) => T, observer: WatchObserver<T> | ((value: T, commit: number) => void), options?: WatchOptions): Watching;
  /** Resolves once no watch is waiting to run again: every commit before the call has been delivered */
  settled(): Promise<void>;
  /** How many watches are open, and what their read sets file */
  stats(): { watches: number; keys: number; ranges: number };
}

export interface WatchScope {
  clock?: WatchClock;
  schedule?: WatchSchedule;
  /** Hears a watch that keeps costing more than a flush's slice, once per watch */
  warn?: (message: string) => void;
}

/** Runs `fn` on a later turn of the event loop, after other work: what a flush that ran out of time waits for */
const later = (fn: () => void) => {
  if (typeof setImmediate === 'function') setImmediate(fn);
  else setTimeout(fn, 0);
};

const PROCESS_CLOCK: WatchClock = {
  now: () => performance.now(),
  // A re-run waiting out its gap is no reason for a process to stay alive
  setTimeout: (fn, ms) => {
    const handle = setTimeout(fn, ms);
    (handle as { unref?: () => void }).unref?.();
    return handle;
  },
  clearTimeout: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};

export function createWatches(connection: Connection, { clock = PROCESS_CLOCK, schedule = WATCH_SCHEDULE, warn }: WatchScope = {}): Watches {
  const open = new Map<number, Watch>();
  /** Tokens name watches to elmdb; one is never reused, so a commit naming a stopped watch's names nothing */
  let nextToken = 1;
  /** Watches waiting to run, in the order they run: those a flush left over first, then by when they were marked */
  let dirty = new Set<Watch>();
  /** A flush queued for a microtask or a later turn, and the timer set for the earliest watch still in its gap */
  let queued = false;
  let timer: { handle: unknown; at: number } | undefined;
  let flushing = false;
  let waiting: Array<() => void> = [];
  let unlisten: (() => void) | undefined;

  const dueAt = (watch: Watch) => (watch.ranAt < 0 ? -Infinity
    : watch.ranAt + Math.max(schedule.minimumGapMs, schedule.delayFactor * watch.costMs));

  /** Queues what the dirty watches need: a flush for those due now, a timer for the first of the rest */
  const plan = (soon: boolean) => {
    const now = clock.now();
    let earliest = Infinity;
    for (const watch of dirty) earliest = Math.min(earliest, dueAt(watch));
    if (earliest === Infinity) return;
    if (earliest <= now) {
      // A flush runs everything due, so a timer standing for one of them would fire on nothing
      if (timer) {
        clock.clearTimeout(timer.handle);
        timer = undefined;
      }
      if (queued) return;
      queued = true;
      if (soon) queueMicrotask(flush);
      else later(flush);
      return;
    }
    if (timer && timer.at <= earliest) return;
    if (timer) clock.clearTimeout(timer.handle);
    timer = {
      at: earliest,
      handle: clock.setTimeout(() => {
        timer = undefined;
        if (closed()) return;
        plan(true);
        settleWaiting();
      }, earliest - now),
    };
  };

  const onCommit = (commit: Commit) => {
    const marked = commit.touched === 'all' ? [...open.values()]
      : [...commit.touched].flatMap((token) => open.get(token) ?? []);
    for (const watch of marked) {
      watch.causes?.add(commit.sequence);
      // A commit landing on a watch already waiting is one whose state the coming run skips
      if (dirty.has(watch)) connection.tally()?.watchSkipped(watch.label);
      else dirty.add(watch);
    }
    // A commit made while a flush runs (a read writing) is planned on a later turn, so a read that writes every time
    // it runs can't keep the microtask queue from ever emptying
    if (marked.length > 0) plan(!flushing);
  };

  const run = (watch: Watch) => {
    // What the token held was the last run's: this run's reads replace it
    connection.untrack(watch.token);
    const started = clock.now();
    const context = watch.causes?.take();
    let outcome: { ok: true; value: unknown } | { ok: false; error: unknown };
    try {
      outcome = { ok: true, value: connection.track(watch.token, () => watch.read(context)) };
    } catch (error) {
      outcome = { ok: false, error };
    }
    watch.ranAt = clock.now();
    watch.costMs = watch.ranAt - started;
    connection.tally()?.watchRun(watch.label, watch.costMs);
    if (watch.stopped) {
      connection.untrack(watch.token);
      return;
    }
    const commit = connection.lastCommit();
    const last = watch.last;
    if (outcome.ok) {
      if (last?.ok && sameValue(last.value, outcome.value)) return;
      watch.last = outcome;
      watch.observer.next(outcome.value, commit);
    } else {
      watch.last = outcome;
      watch.observer.error?.(outcome.error, commit);
    }
  };

  /** Notes a run that took more than the slice, warning once about a watch that keeps doing it */
  const charge = (watch: Watch) => {
    if (watch.costMs <= schedule.flushSliceMs) {
      watch.overBudget = 0;
      return;
    }
    watch.overBudget++;
    if (warn && !watch.warned && watch.overBudget >= OVER_BUDGET_WARNING) {
      watch.warned = true;
      warn(`A watch takes ${watch.costMs.toFixed(1)} ms a run, more than a flush's ${schedule.flushSliceMs} ms, `
        + `${watch.overBudget} runs in a row: ${watch.label}. It runs at most every `
        + `${(schedule.delayFactor * watch.costMs).toFixed(0)} ms; a narrower read costs less`);
    }
  };

  /**
   * Whether the database was closed under the watches: its owner closes it, never the engine, so the scheduler learns
   * of it the next time it would run one. It then stops them all, as each one's `stop` would, delivering nothing
   */
  function closed(): boolean {
    if (connection.database.isOpen) return false;
    for (const watch of open.values()) watch.stopped = true;
    open.clear();
    dirty.clear();
    unlisten?.();
    unlisten = undefined;
    if (timer) {
      clock.clearTimeout(timer.handle);
      timer = undefined;
    }
    queued = false;
    settleWaiting();
    return true;
  }

  function flush() {
    queued = false;
    if (closed()) return;
    flushing = true;
    const started = clock.now();
    // Commits the runs make mark the next set, so nothing marked here runs twice in this flush
    const batch = dirty;
    dirty = new Set();
    const left: Watch[] = [];
    let ranOne = false;
    try {
      for (const watch of batch) {
        if (watch.stopped) continue;
        const now = clock.now();
        const spent = now - started;
        // In its gap; or, once another has run — a flush always runs one, so every flush makes progress — out of time,
        // or a run its last cost says would overrun what remains
        if (dueAt(watch) > now || (ranOne && spent + watch.costMs > schedule.flushSliceMs)) {
          left.push(watch);
          continue;
        }
        ranOne = true;
        // A commit an earlier run in this flush made is one this run reads: it needn't run again for it
        if (dirty.delete(watch)) connection.tally()?.watchSkipped(watch.label);
        try {
          run(watch);
          charge(watch);
        } catch (error) {
          // An observer's (or a warning's) own throw: reported, without stopping the other watches' deliveries
          queueMicrotask(() => { throw error; });
        }
      }
    } finally {
      flushing = false;
      // Those left over run first next time, then those this flush's commits marked; one stopped meanwhile doesn't
      dirty = new Set([...left.filter((watch) => !watch.stopped), ...dirty]);
    }
    if (dirty.size > 0) plan(false);
    settleWaiting();
  }

  /** Resolves `settled()`'s callers once nothing waits to run: no watch dirty, no flush queued and no timer set */
  function settleWaiting() {
    if (dirty.size > 0 || queued || timer) return;
    const resolve = waiting;
    waiting = [];
    for (const done of resolve) done();
  }

  return {
    watch(read, observer, options) {
      const watch: Watch = {
        token: nextToken++,
        label: options?.label ?? callSite(),
        read,
        causes: options?.recordCauses && new WatchCauses(options.recordCauses),
        observer: (typeof observer === 'function' ? { next: observer } : observer) as WatchObserver<unknown>,
        last: undefined,
        stopped: false,
        ranAt: -1,
        costMs: 0,
        overBudget: 0,
        warned: false,
      };
      open.set(watch.token, watch);
      unlisten ??= connection.onCommit(onCommit);
      dirty.add(watch);
      plan(!flushing);
      return {
        stop() {
          if (watch.stopped) return;
          watch.stopped = true;
          open.delete(watch.token);
          dirty.delete(watch);
          connection.untrack(watch.token);
          if (open.size === 0) {
            unlisten?.();
            unlisten = undefined;
          }
          if (dirty.size === 0 && timer) {
            clock.clearTimeout(timer.handle);
            timer = undefined;
          }
          if (!flushing) settleWaiting();
        },
      };
    },
    settled() {
      // A flush under way still holds watches it hasn't run, which it settles when it ends
      if (!flushing && !queued && !timer && dirty.size === 0) return Promise.resolve();
      return new Promise((resolve) => { waiting.push(resolve); });
    },
    stats: () => {
      const { keys, ranges } = connection.trackStats();
      return { watches: open.size, keys, ranges };
    },
  };
}

/** Whether two results are the same data: primitives, lists, plain objects, maps, sets, bytes and dates, compared through */
export function sameValue(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false;
  if (Array.isArray(a)) {
    if (!Array.isArray(b) || a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (!sameValue(a[i], b[i])) return false;
    return true;
  }
  if (Array.isArray(b)) return false;
  if (a instanceof Date || b instanceof Date) return a instanceof Date && b instanceof Date && a.getTime() === b.getTime();
  if (ArrayBuffer.isView(a) || ArrayBuffer.isView(b)) {
    if (!ArrayBuffer.isView(a) || !ArrayBuffer.isView(b) || a.byteLength !== b.byteLength) return false;
    const x = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
    const y = new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
    for (let i = 0; i < x.length; i++) if (x[i] !== y[i]) return false;
    return true;
  }
  if (Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
  // Keys of a map and members of a set are compared as the collection itself finds them, by identity
  if (a instanceof Map && b instanceof Map) {
    if (a.size !== b.size) return false;
    for (const [key, value] of a) if (!b.has(key) || !sameValue(value, b.get(key))) return false;
    return true;
  }
  if (a instanceof Set && b instanceof Set) {
    if (a.size !== b.size) return false;
    for (const member of a) if (!b.has(member)) return false;
    return true;
  }
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) return false;
  for (const key of keys) {
    if (!Object.prototype.hasOwnProperty.call(b, key)) return false;
    if (!sameValue((a as Record<string, unknown>)[key], (b as Record<string, unknown>)[key])) return false;
  }
  return true;
}
