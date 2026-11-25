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
import type { EARSRuntimeDeps } from './runtime.ts';

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
  /**
   * Resolves once the watch has been handed everything this engine committed before the call: at once when it has, and
   * otherwise when the run that is owed has ended, whatever that run delivered. It waits for this watch and no
   * other, so it resolves under a stream of commits that keeps other watches behind, where `watchesSettled` waits
   * for a moment with none behind, which may not come. It resolves too when the watch is stopped or its database
   * closed, with nothing more to be delivered. A commit another process made is one the engine hears of later, so
   * a wait may resolve before it is handed over.
   *
   * `soon` says something is blocked on the answer. The run that is owed is then held to the gap its own cost
   * sets and not to the floor of a frame, and goes first in its flush, brought forward if it was put off to a later turn:
   * a read that costs a tenth of a millisecond is due four tenths after its last run, where it would have waited
   * sixteen, though the process's timers fire no sooner than a millisecond. The floor is for a watch nobody is
   * waiting on, where a frame is as soon as anyone looks. The factor is what bounds a watch's share of the thread,
   * and it stays. Without `soon` a wait changes nothing about when the watch runs.
   */
  caughtUp(options?: { soon?: boolean }): Promise<void>;
}

/** The clock a scheduler reads and the timers it sets: the process's, or a test's */
export interface WatchClock {
  now(): number;
  /** `held` says something is waiting on what the timer is for: the process's clock then keeps the process alive for it */
  setTimeout(fn: () => void, ms: number, held?: boolean): unknown;
  clearTimeout(handle: unknown): void;
}

/**
 * The scheduler's constants. A watch's gap after a run is `delayFactor` times that run's time, and at least
 * `minimumGapMs`; a flush starts no run past `flushSliceMs` of runs. Set by the stress reading
 * (`bench/watch-stress.ts`; elmdb's docs/evidence/next-phase-3.md)
 */
export interface WatchSchedule {
  readonly delayFactor: number;
  readonly minimumGapMs: number;
  readonly flushSliceMs: number;
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
  /** Whether a run is owed: a commit wrote into what it read since its last run began, or it has never run */
  owed: boolean;
  /** What `caughtUp` is waiting to resolve, once the run that is owed has ended */
  waiters: Array<() => void>;
  /** Whether something blocked is waiting on the run that is owed, which is then not held to the floor */
  soon: boolean;
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
  warn?: EARSRuntimeDeps['warn'];
}

/** Runs `fn` on a later turn of the event loop, after other work: what a flush that ran out of time waits for */
const later = (fn: () => void) => {
  if (typeof setImmediate === 'function') setImmediate(fn);
  else setTimeout(fn, 0);
};

const PROCESS_CLOCK: WatchClock = {
  now: () => performance.now(),
  // A re-run waiting out its gap is no reason for a process to stay alive, unless something waits on the run
  setTimeout: (fn, ms, held) => {
    const handle = setTimeout(fn, ms);
    if (!held) (handle as { unref?: () => void }).unref?.();
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
  /** Earliest `dueAt` among `dirty` watches, or `undefined` when invalidated by removing a minimum watch */
  let earliestDue: number | undefined = Infinity;
  /**
   * A flush queued, for a microtask or for a later turn of the event loop, and the timer set for the earliest watch
   * still in its gap. A flush a sooner one replaced still comes, and finds whatever is left to it, often nothing
   */
  let queued: { later: boolean } | undefined;
  let timer: { handle: unknown; at: number; held: boolean } | undefined;
  /** How many `caughtUp` calls wait, over every watch: while any does, or `settled` is waited on, the timer for a gap is one to stay alive for */
  let caughtUpWaiting = 0;
  let flushing = false;
  let waiting: Array<() => void> = [];
  let unlisten: (() => void) | undefined;

  const dueAt = (watch: Watch) => (watch.ranAt < 0 ? -Infinity
    : watch.ranAt + Math.max(watch.soon ? 0 : schedule.minimumGapMs, schedule.delayFactor * watch.costMs));

  const addDirty = (watch: Watch) => {
    dirty.add(watch);
    if (earliestDue !== undefined) {
      const due = dueAt(watch);
      if (due < earliestDue) earliestDue = due;
    }
  };

  const earliestDirty = (): number => {
    if (earliestDue !== undefined) return earliestDue;
    let min = Infinity;
    for (const watch of dirty) {
      const due = dueAt(watch);
      if (due < min) min = due;
    }
    return earliestDue = min;
  };

  /** Queues what the dirty watches need: a flush for those due now, a timer for the first of the rest */
  const plan = (soon: boolean, blocked = false) => {
    const earliest = earliestDirty();
    if (earliest === Infinity) return;
    const now = clock.now();
    if (earliest <= now) {
      // A flush runs everything due, so a timer standing for one of them would fire on nothing
      if (timer) {
        clock.clearTimeout(timer.handle);
        timer = undefined;
      }
      // A flush put off to a later turn was put off for other work's sake. Something blocked on a watch that is due
      // is not other work: its flush comes now, and the one put off finds nothing of its own to do
      if (queued && !(blocked && queued.later)) return;
      queued = { later: !soon };
      if (soon) queueMicrotask(flush);
      else later(flush);
      return;
    }
    const held = caughtUpWaiting > 0 || waiting.length > 0;
    if (timer && timer.at <= earliest && timer.held === held) return;
    if (timer) clock.clearTimeout(timer.handle);
    timer = {
      at: earliest,
      held,
      handle: clock.setTimeout(() => {
        timer = undefined;
        if (closed()) return;
        plan(true);
        settleWaiting();
      }, earliest - now, held),
    };
  };

  const onCommit = (commit: Commit) => {
    let any = false;
    const mark = (watch: Watch) => {
      any = true;
      watch.owed = true;
      watch.causes?.add(commit.sequence);
      // A commit landing on a watch already waiting is one whose state the coming run skips
      if (dirty.has(watch)) connection.tally()?.watchSkipped(watch.label);
      else addDirty(watch);
    };
    if (commit.touched === 'all') {
      for (const watch of open.values()) mark(watch);
    } else {
      for (const token of commit.touched) {
        const watch = open.get(token);
        if (watch) mark(watch);
      }
    }
    // A commit made while a flush runs (a read writing) is planned on a later turn, so a read that writes every time
    // it runs can't keep the microtask queue from ever emptying
    if (any) plan(!flushing);
  };

  /** Resolves `caughtUp` calls: each is answered once, by the run it waited for or by the watch ending */
  const answer = (waiters: Array<() => void>) => {
    caughtUpWaiting -= waiters.length;
    for (const caughtUp of waiters) caughtUp();
  };

  const run = (watch: Watch) => {
    // What waited before this run began is answered by it, however it ends. A commit landing while it runs is
    // owed a run of its own, and what waits from then on waits for that one
    watch.owed = false;
    // What was blocked is answered by this run: the next one has its whole gap unless something waits again
    watch.soon = false;
    const waited = watch.waiters;
    watch.waiters = [];
    try {
      runOnce(watch);
    } finally {
      answer(waited);
    }
  };

  const runOnce = (watch: Watch) => {
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
    if (closed()) return;
    if (watch.stopped) {
      connection.untrack(watch.token);
      return;
    }
    let commit: number;
    try {
      commit = connection.lastCommit();
    } catch (error) {
      if ((error as { code?: unknown })?.code === 'CLOSED' && closed()) return;
      throw error;
    }
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
        + `${(schedule.delayFactor * watch.costMs).toFixed(0)} ms; a narrower read costs less`,
      { kind: 'slowWatch', label: watch.label, ms: watch.costMs, runs: watch.overBudget });
    }
  };

  /**
   * Whether the database was closed under the watches: its owner closes it, never the engine, so the scheduler learns
   * of it the next time it would run one. It then stops them all, as each one's `stop` would, delivering nothing
   */
  function closed(): boolean {
    if (connection.database.isOpen) return false;
    for (const watch of open.values()) {
      watch.stopped = true;
      answer(watch.waiters.splice(0));
    }
    open.clear();
    dirty.clear();
    earliestDue = Infinity;
    unlisten?.();
    unlisten = undefined;
    if (timer) {
      clock.clearTimeout(timer.handle);
      timer = undefined;
    }
    queued = undefined;
    settleWaiting();
    return true;
  }

  function flush() {
    queued = undefined;
    if (closed()) return;
    flushing = true;
    const started = clock.now();
    // Commits the runs make mark the next set, so nothing marked here runs twice in this flush
    // Those something blocked is waiting on go first, so the slice is spent on them before any other
    const batch = [...dirty].sort((a, b) => Number(b.soon) - Number(a.soon));
    dirty = new Set();
    earliestDue = Infinity;
    const left: Watch[] = [];
    let ranOne = false;
    try {
      for (const watch of batch) {
        if (closed()) break;
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
          if (closed()) break;
          charge(watch);
        } catch (error) {
          // An observer's (or a warning's) own throw: reported, without stopping the other watches' deliveries
          queueMicrotask(() => { throw error; });
        }
      }
    } finally {
      flushing = false;
      if (closed()) {
        dirty.clear();
        earliestDue = Infinity;
      } else {
        // Those left over run first next time, then those this flush's commits marked; one stopped meanwhile doesn't
        const nextDirty = new Set<Watch>();
        let min = Infinity;
        for (const watch of left) {
          if (watch.stopped) continue;
          nextDirty.add(watch);
          const due = dueAt(watch);
          if (due < min) min = due;
        }
        for (const watch of dirty) {
          if (watch.stopped) continue;
          nextDirty.add(watch);
          const due = dueAt(watch);
          if (due < min) min = due;
        }
        dirty = nextDirty;
        earliestDue = min;
      }
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
        label: options?.label ?? connection.callSite(),
        read,
        causes: options?.recordCauses && new WatchCauses(options.recordCauses),
        observer: (typeof observer === 'function' ? { next: observer } : observer) as WatchObserver<unknown>,
        last: undefined,
        stopped: false,
        ranAt: -1,
        costMs: 0,
        overBudget: 0,
        warned: false,
        owed: true,
        waiters: [],
        soon: false,
      };
      open.set(watch.token, watch);
      unlisten ??= connection.onCommit(onCommit);
      addDirty(watch);
      plan(!flushing);
      return {
        stop() {
          if (watch.stopped) return;
          watch.stopped = true;
          answer(watch.waiters.splice(0));
          open.delete(watch.token);
          if (dirty.delete(watch)) {
            if (dirty.size === 0) earliestDue = Infinity;
            else if (earliestDue !== undefined && dueAt(watch) <= earliestDue) earliestDue = undefined;
          }
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
        caughtUp(options) {
          if (watch.stopped || !watch.owed) return Promise.resolve();
          return new Promise((resolve) => {
            watch.waiters.push(resolve);
            caughtUpWaiting++;
            if (options?.soon) {
              watch.soon = true;
              if (dirty.has(watch) && earliestDue !== undefined) {
                const due = dueAt(watch);
                if (due < earliestDue) earliestDue = due;
              }
            }
            // A timer set for its gap before anything waited is set again: as one to stay alive for, and for
            // sooner where the wait lifted the floor
            if (!flushing) plan(true, watch.soon);
          });
        },
      };
    },
    settled() {
      // A flush under way still holds watches it hasn't run, which it settles when it ends
      if (!flushing && !queued && !timer && dirty.size === 0) return Promise.resolve();
      return new Promise((resolve) => {
        waiting.push(resolve);
        // A timer set for a gap before anything waited is set again, as one to stay alive for
        if (!flushing) plan(true);
      });
    },
    stats: () => {
      const { keys, ranges } = connection.trackStats();
      return { watches: open.size, keys, ranges };
    },
  };
}

function sameValueInner(a: unknown, b: unknown, seen?: WeakMap<object, Set<object>>): boolean {
  if (Object.is(a, b)) return true;
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false;
  const proto = Object.getPrototypeOf(a);
  if (proto !== Object.getPrototypeOf(b)) return false;
  if (a instanceof Date) return Object.is(a.getTime(), (b as Date).getTime());
  if (a instanceof RegExp) {
    const rb = b as RegExp;
    return a.source === rb.source && a.flags === rb.flags && a.lastIndex === rb.lastIndex;
  }
  if (a instanceof URL) return a.href === (b as URL).href;
  if (ArrayBuffer.isView(a)) {
    const vb = b as ArrayBufferView;
    if (a.byteLength !== vb.byteLength) return false;
    const x = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
    const y = new Uint8Array(vb.buffer, vb.byteOffset, vb.byteLength);
    for (let i = 0; i < x.length; i++) if (x[i] !== y[i]) return false;
    return true;
  }
  if (a instanceof ArrayBuffer) {
    const bb = b as ArrayBuffer;
    if (a.byteLength !== bb.byteLength) return false;
    const x = new Uint8Array(a);
    const y = new Uint8Array(bb);
    for (let i = 0; i < x.length; i++) if (x[i] !== y[i]) return false;
    return true;
  }
  // Members of a set are compared as the collection itself finds them, by identity
  if (a instanceof Set) {
    const sb = b as Set<unknown>;
    if (a.size !== sb.size) return false;
    for (const member of a) if (!sb.has(member)) return false;
    return true;
  }
  const isArray = Array.isArray(a);
  const isMap = !isArray && a instanceof Map;
  // Distinct non-plain objects (Errors, WeakMaps, Promises, custom instances) are never equal by empty own keys
  if (!isArray && !isMap && proto !== Object.prototype && proto !== null) return false;

  let pairs = seen?.get(a);
  if (pairs?.has(b)) return true;
  const nextSeen = seen ?? new WeakMap<object, Set<object>>();
  if (!pairs) nextSeen.set(a, pairs = new Set());
  pairs.add(b);

  if (isArray) {
    const ab = b as unknown[];
    if (a.length !== ab.length) return false;
    for (let i = 0; i < a.length; i++) if (!sameValueInner(a[i], ab[i], nextSeen)) return false;
    return true;
  }
  // Keys of a map are compared as the collection itself finds them, by identity
  if (isMap) {
    const mb = b as Map<unknown, unknown>;
    if (a.size !== mb.size) return false;
    for (const [key, value] of a) if (!mb.has(key) || !sameValueInner(value, mb.get(key), nextSeen)) return false;
    return true;
  }
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) return false;
  for (const key of keys) {
    if (!Object.prototype.hasOwnProperty.call(b, key)) return false;
    if (!sameValueInner((a as Record<string, unknown>)[key], (b as Record<string, unknown>)[key], nextSeen)) return false;
  }
  return true;
}

/** Whether two results are the same data: primitives, lists, plain objects, maps, sets, bytes, regexes, URLs and dates, compared through */
export function sameValue(a: unknown, b: unknown): boolean {
  return sameValueInner(a, b);
}
