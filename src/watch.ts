// Live reads: `watch` runs a read, records what it read (`OrderedKV.track`), and runs it again after each commit that
// wrote into that, handing the listener the result when it changed. A commit's keys find the watches they touch
// through the read-set index, the re-runs wait until the writing code has returned (a microtask), so a burst of commits
// costs one, and a result equal to the last delivered is not delivered. A read that consults anything but the store
// isn't tracked, and goes stale when that changes: a watched read is a pure read of the engine.
import type { Commit, OrderedKV } from './kv.ts';
import { createReadSetIndex } from './read-set-index.ts';

/** What a watch delivers to: each changed result, and what a read threw, with the commit it reflects */
export interface WatchObserver<T> {
  /** A result that differs from the last one delivered (the first, always) */
  next(value: T, commit: number): void;
  /** What the read threw; it stays watched, and runs again after a commit that touches what it read before throwing */
  error?(error: unknown, commit: number): void;
}

/** An open watch */
export interface Watching {
  /** Ends the watch: nothing is delivered after it */
  stop(): void;
}

/** A watch as the registry holds it */
interface Watch {
  read: () => unknown;
  observer: WatchObserver<unknown>;
  /** What was last delivered: a value, an error, or nothing yet */
  last: { ok: true; value: unknown } | { ok: false; error: unknown } | undefined;
  stopped: boolean;
}

export interface Watches {
  watch<T>(read: () => T, observer: WatchObserver<T> | ((value: T, commit: number) => void)): Watching;
  /** Resolves once no watch is waiting to run again: every commit before the call has been delivered */
  settled(): Promise<void>;
  /** How many watches are open, and what their read sets file */
  stats(): { watches: number; keys: number; ranges: number };
}

/** Runs `fn` on a later turn of the event loop, after other work: what a flush that touched itself waits for */
const later = (fn: () => void) => {
  if (typeof setImmediate === 'function') setImmediate(fn);
  else setTimeout(fn, 0);
};

export function createWatches(kv: OrderedKV): Watches {
  const index = createReadSetIndex<Watch>();
  const open = new Set<Watch>();
  /** Watches waiting to run, and whether a flush is scheduled */
  let dirty = new Set<Watch>();
  let scheduled = false;
  let flushing = false;
  let waiting: Array<() => void> = [];
  let unlisten: (() => void) | undefined;

  const schedule = (soon: boolean) => {
    if (scheduled) return;
    scheduled = true;
    if (soon) queueMicrotask(flush);
    else later(flush);
  };

  const onCommit = (commit: Commit) => {
    for (const watch of index.touchedBy(commit.keys)) dirty.add(watch);
    // A commit made while a flush runs (a read building an index) runs on a later turn, so a read that writes every
    // time it runs can't keep the microtask queue from ever emptying
    if (dirty.size > 0) schedule(!flushing);
  };

  const run = (watch: Watch) => {
    const outcome = kv.track(watch.read);
    if (watch.stopped) return;
    index.set(watch, outcome.reads);
    const commit = kv.lastCommit;
    const last = watch.last;
    if (outcome.ok) {
      if (last?.ok && sameValue(last.value, outcome.value)) return;
      watch.last = { ok: true, value: outcome.value };
      watch.observer.next(outcome.value, commit);
    } else {
      watch.last = { ok: false, error: outcome.error };
      watch.observer.error?.(outcome.error, commit);
    }
  };

  function flush() {
    scheduled = false;
    flushing = true;
    try {
      const now = dirty;
      dirty = new Set();
      for (const watch of now) {
        if (watch.stopped) continue;
        try {
          run(watch);
        } catch (error) {
          // An observer's own throw: reported, without stopping the other watches' deliveries
          queueMicrotask(() => { throw error; });
        }
      }
    } finally {
      flushing = false;
    }
    if (dirty.size > 0) schedule(false);
    else if (!scheduled) {
      const resolve = waiting;
      waiting = [];
      for (const done of resolve) done();
    }
  }

  return {
    watch(read, observer) {
      const watch: Watch = {
        read,
        observer: (typeof observer === 'function' ? { next: observer } : observer) as WatchObserver<unknown>,
        last: undefined,
        stopped: false,
      };
      open.add(watch);
      unlisten ??= kv.onCommit(onCommit);
      dirty.add(watch);
      schedule(!flushing);
      return {
        stop() {
          if (watch.stopped) return;
          watch.stopped = true;
          open.delete(watch);
          dirty.delete(watch);
          index.remove(watch);
          if (open.size === 0) {
            unlisten?.();
            unlisten = undefined;
          }
        },
      };
    },
    settled() {
      if (!scheduled && dirty.size === 0) return Promise.resolve();
      return new Promise((resolve) => { waiting.push(resolve); });
    },
    stats: () => ({ watches: open.size, ...index.size() }),
  };
}

/** Whether two results are the same data: primitives, lists, plain objects, bytes and dates, compared through */
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
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) return false;
  for (const key of keys) {
    if (!Object.prototype.hasOwnProperty.call(b, key)) return false;
    if (!sameValue((a as Record<string, unknown>)[key], (b as Record<string, unknown>)[key])) return false;
  }
  return true;
}
