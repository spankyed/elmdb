import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { afterEach, describe, expect, it } from 'vitest';
import { EarsError, createEarsEngine, createSystemClock, type EarsClock, type Schema } from '../src/index.ts';
import { createTemporaryDatabases } from '../src/lmdb/index.ts';

const require = createRequire(import.meta.url);
const nativeElmdbPath = require.resolve('elmdb');
const clientElmdbPath = require.resolve('elmdb/client');

const watchSchema: Schema = {
  types: {
    Task: {
      fields: {
        title: { type: 'string' },
        done: { type: 'boolean' },
      },
      indexes: {
        byDone: { kind: 'value', fields: ['done'] },
      },
    },
    Other: {
      fields: { name: { type: 'string' } },
    },
  },
  relations: {},
};

interface FakeClock extends EarsClock {
  advance(ms: number): void;
  readonly pendingTimers: number;
}

function createFakeClock(initialMs = 1_000): FakeClock {
  let now = initialMs;
  let nextId = 1;
  const timers = new Map<number, { dueAt: number; fn: () => void }>();

  return {
    now: () => now,
    setTimeout(callback, delayMs) {
      const id = nextId++;
      timers.set(id, { dueAt: now + Math.max(0, delayMs), fn: callback });
      return id;
    },
    clearTimeout(handle) {
      timers.delete(handle as number);
    },
    get pendingTimers(): number {
      return timers.size;
    },
    advance(ms: number): void {
      const target = now + ms;
      while (true) {
        let earliestId: number | undefined;
        let earliestDue = Infinity;
        for (const [id, entry] of timers.entries()) {
          if (entry.dueAt <= target && entry.dueAt < earliestDue) {
            earliestDue = entry.dueAt;
            earliestId = id;
          }
        }
        if (earliestId === undefined) break;
        const entry = timers.get(earliestId)!;
        timers.delete(earliestId);
        now = entry.dueAt;
        entry.fn();
      }
      now = target;
    },
  };
}

const temps = createTemporaryDatabases();

afterEach(() => {
  temps.disposeAll();
});

describe('@elmdb/ears sans-I/O WatchPump', () => {
  it('delivers initial state, selectively invalidates, deduplicates declarative results, and catches up cross-handle commits', () => {
    const handle = temps.open({ schema: watchSchema, durability: 'strict' });
    const clock = createFakeClock(10_000);
    const engine = createEarsEngine({ database: handle.database, clock });

    const snapshots: string[][] = [];
    const counts: number[] = [];

    const w1 = engine.query
      .of('Task')
      .where('done', false)
      .orderById('asc')
      .watch((rows) => snapshots.push(rows.map((r) => r.id)), { minGapMs: 10 });

    const w2 = engine.query
      .of('Task')
      .where('done', false)
      .watchCount((c) => counts.push(c), { minGapMs: 10 });

    expect(w1.active).toBe(true);
    expect(engine.watches.activeCount).toBe(2);
    expect(snapshots).toEqual([[]]);
    expect(counts).toEqual([0]);
    expect(engine.watches.settled()).toBe(true);

    // Unrelated entity write must NOT dirty Task watches
    engine.create('Other', { name: 'ignore me' }, 'o1');
    clock.advance(50);
    expect(snapshots).toEqual([[]]);
    expect(counts).toEqual([0]);

    // Matching write dirties and delivers after minGapMs
    engine.create('Task', { title: 'One', done: false }, 't1');
    expect(engine.watches.settled()).toBe(false);
    clock.advance(20);
    expect(snapshots).toEqual([[], ['t1']]);
    expect(counts).toEqual([0, 1]);
    expect(engine.watches.settled()).toBe(true);

    // Declarative dedup: changing a field that does not change the matching count must not re-fire watchCount
    engine.patch('t1', { title: 'One renamed' });
    clock.advance(20);
    expect(snapshots).toHaveLength(3); // rows changed (title changed)
    expect(counts).toEqual([0, 1]); // count stayed 1 -> deduplicated!

    // Real cross-process commit caught up via persisted commit_log
    execFileSync(
      process.execPath,
      [
        '-e',
        `const { Database } = require(${JSON.stringify(nativeElmdbPath)});
         const { buildWireBatch } = require(${JSON.stringify(clientElmdbPath)});
         const db = Database.open(${JSON.stringify(handle.path)}, { durability: 'strict' });
         try {
           db.executeWireBatch(buildWireBatch((b) => { b.create('Task', { title: 'From other process', done: false }, 't2'); }));
         } finally {
           db.close();
         }`,
      ],
      { stdio: 'inherit' },
    );

    const dirtied = engine.watches.catchUpCrossProcess();
    expect(dirtied.sort((a, b) => a - b)).toEqual([w1.id, w2.id].sort((a, b) => a - b));
    clock.advance(20);
    expect(snapshots[snapshots.length - 1]).toEqual(['t1', 't2']);
    expect(counts[counts.length - 1]).toBe(2);
    expect(engine.watches.caughtUp()).toBe(true);

    w1.stop();
    expect(w1.active).toBe(false);
    expect(engine.watches.activeCount).toBe(1);
    engine.watches.stopAll();
    expect(engine.watches.activeCount).toBe(0);
    expect(clock.pendingTimers).toBe(0);
  });

  it('supports immediate: false, imperative mode, flushSync, error isolation, and self-stopping callbacks', () => {
    const handle = temps.open({ schema: watchSchema });
    const clock = createFakeClock(5_000);
    const errors: EarsError[] = [];
    const engine = createEarsEngine({
      database: handle.database,
      clock,
      onWatchError: (e) => errors.push(e),
    });

    const delivered: string[][] = [];
    let selfHandle: { stop(): void } | undefined;
    selfHandle = engine.query
      .of('Task')
      .watchIds(
        (ids) => {
          delivered.push(ids);
          if (ids.length === 1) selfHandle?.stop();
        },
        { immediate: false, mode: 'imperative', minGapMs: 0 },
      );

    expect(delivered).toEqual([]);

    const throwingErrors: EarsError[] = [];
    const throwingWatch = engine.query.of('Task').watchCount(
      () => {
        throw new Error('boom in subscriber');
      },
      { immediate: true, onError: (e) => throwingErrors.push(e) },
    );
    expect(throwingErrors).toHaveLength(1);
    throwingWatch.stop();

    engine.create('Task', { title: 'Trigger', done: false }, 't1');
    const ran = engine.watches.flushSync(5_050);
    expect(ran).toBe(1);
    expect(delivered).toEqual([['t1']]);
    expect(engine.watches.activeCount).toBe(0);
    expect(errors).toEqual([]);
  });

  it('prevents livelock on schema-broken watches, clears stale timers after flushSync, and cleans up failed immediate registration', () => {
    const handle = temps.open({ schema: watchSchema });
    const clock = createFakeClock(20_000);
    const engine = createEarsEngine({ database: handle.database, clock });

    const brokenErrors: EarsError[] = [];
    const healthySnapshots: number[] = [];

    const broken = engine.query
      .of('Task')
      .where('title', 'hello')
      .watchIds(() => {}, { minGapMs: 0, onError: (e) => brokenErrors.push(e) });
    const healthy = engine.query
      .of('Task')
      .where('done', false)
      .watchCount((c) => healthySnapshots.push(c), { minGapMs: 0 });

    // Rename 'title' -> 'heading' breaks `broken` watch permanently; healthy watch stays valid!
    engine.admin.renameField('Task', 'title', 'heading');
    engine.create('Task', { heading: 'alive', done: false }, 't-alive');

    // Advancing clock must NOT hang (zero 0ms livelock!) and must report the schema error exactly once.
    clock.advance(100);
    expect(brokenErrors).toHaveLength(1);
    expect(brokenErrors[0]?.code).toBe('UNKNOWN_FIELD');
    expect(broken.active).toBe(false);
    expect(healthy.active).toBe(true);
    expect(healthySnapshots).toEqual([0, 1]);
    expect(clock.pendingTimers).toBe(0);

    // flushSync clears pending timers when settled
    engine.create('Task', { heading: 'second', done: false }, 't-second');
    expect(clock.pendingTimers).toBe(1);
    engine.watches.flushSync(clock.now() + 50);
    expect(healthySnapshots).toEqual([0, 1, 2]);
    expect(clock.pendingTimers).toBe(0);

    // Re-entrant mutation inside flushSync schedules a timer for cooldown watches instead of dropping it
    const reentrantCounts: number[] = [];
    let didMutate = false;
    const reentrant = engine.query.of('Task').watchCount(
      (c) => {
        reentrantCounts.push(c);
        if (!didMutate && c === 3) {
          didMutate = true;
          engine.create('Task', { heading: 'from-callback', done: false }, 't-reentrant');
        }
      },
      { minGapMs: 25 },
    );
    engine.create('Task', { heading: 'trigger-reentrant', done: false }, 't-trigger');
    engine.watches.flushSync(clock.now() + 100);
    expect(reentrantCounts).toEqual([2, 3]);
    expect(clock.pendingTimers).toBe(1);
    clock.advance(200);
    expect(reentrantCounts).toEqual([2, 3, 4]);
    reentrant.stop();

    // Throwing from onError during immediate registration must NOT leak the watch!
    const beforeCount = engine.watches.activeCount;
    expect(() =>
      engine.query.of('Task').watchCount(
        () => {
          throw new Error('initial boom');
        },
        {
          immediate: true,
          onError: (err) => {
            throw err;
          },
        },
      ),
    ).toThrow('initial boom');
    expect(engine.watches.activeCount).toBe(beforeCount);

    healthy.stop();
  });

  it('stopAll unregisters active watches without permanently poisoning the Rust WatchScheduler', () => {
    const handle = temps.open({ schema: watchSchema });
    const clock = createFakeClock(30_000);
    const engine = createEarsEngine({ database: handle.database, clock });

    const firstSnapshots: number[] = [];
    const first = engine.query.of('Task').watchCount((c) => firstSnapshots.push(c), { minGapMs: 0 });
    expect(firstSnapshots).toEqual([0]);
    expect(engine.watches.activeCount).toBe(1);

    engine.watches.stopAll();
    expect(first.active).toBe(false);
    expect(engine.watches.activeCount).toBe(0);

    // Mutating after stopAll must not deliver to the stopped watch.
    engine.create('Task', { title: 'after-stop', done: false }, 't-after-stop');
    clock.advance(50);
    expect(firstSnapshots).toEqual([0]);

    // Registering a brand-new watch after stopAll MUST still receive live updates!
    const secondSnapshots: number[] = [];
    const second = engine.query.of('Task').watchCount((c) => secondSnapshots.push(c), { minGapMs: 0 });
    expect(secondSnapshots).toEqual([1]);
    expect(engine.watches.activeCount).toBe(1);

    engine.create('Task', { title: 'live-again', done: false }, 't-live-again');
    clock.advance(50);
    expect(secondSnapshots).toEqual([1, 2]);
    expect(engine.watches.settled()).toBe(true);

    second.stop();

    // Closing the database permanently stops watches; registering a watch after close throws CLOSED.
    handle.database.close();
    expect(() => engine.query.of('Task').watchCount(() => {})).toThrowError(
      expect.objectContaining({ code: 'CLOSED' }),
    );
  });

  it('supports AfterClock (@apackdev/clock shape), onSettled(), non-stalling backoff, and 8ms turn-slicing while another watch is backed off (P1-16, P2-10)', () => {
    // P2-10: Fake AfterClock (`now()` + `after(ms, run) => cancel`)
    let nowMs = 50_000;
    let nextTimerId = 1;
    const timers = new Map<number, { at: number; run: () => void }>();
    const afterClock = {
      now: () => nowMs,
      after: (ms: number, run: () => void) => {
        const id = nextTimerId++;
        timers.set(id, { at: nowMs + ms, run });
        return () => {
          timers.delete(id);
        };
      },
    };
    const advanceAfter = (deltaMs: number): void => {
      const target = nowMs + deltaMs;
      while (true) {
        let earliestId: number | undefined;
        let earliestAt = Infinity;
        for (const [id, t] of timers) {
          if (t.at <= target && t.at < earliestAt) {
            earliestAt = t.at;
            earliestId = id;
          }
        }
        if (earliestId === undefined) break;
        const entry = timers.get(earliestId)!;
        timers.delete(earliestId);
        nowMs = Math.max(nowMs, entry.at);
        entry.run();
      }
      nowMs = Math.max(nowMs, target);
    };

    const handle = temps.open({ schema: watchSchema });
    // Wrap database so we can inject transient errors on a specific watch ID and simulate slow reruns.
    const realDb = handle.database;
    let transientFailWatchId: number | undefined;
    let rerunCostStepMs = 0;
    const wrappedDb = new Proxy(realDb, {
      get(target, prop, receiver) {
        if (prop === 'rerunWatch') {
          return (watchId: number, spec: unknown, now: number, costMs: number) => {
            if (watchId === transientFailWatchId) {
              throw new EarsError('IO', 'transient simulated I/O error');
            }
            if (rerunCostStepMs > 0) {
              nowMs += rerunCostStepMs;
            }
            return target.rerunWatch(watchId, spec as Parameters<typeof target.rerunWatch>[1], now, costMs);
          };
        }
        const value = Reflect.get(target, prop, receiver);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });

    const engine = createEarsEngine({ database: wrappedDb, clock: afterClock });

    // P2-10: onSettled() fires immediately when already settled!
    let immediateSettled = 0;
    engine.watches.onSettled(() => {
      immediateSettled++;
    });
    expect(immediateSettled).toBe(1);

    const failingErrors: EarsError[] = [];
    const healthyCounts: number[] = [];
    const failingWatch = engine.query.of('Task').watchCount(() => {}, {
      minGapMs: 0,
      onError: (e) => failingErrors.push(e),
    });
    const healthyWatch = engine.query.of('Task').watchCount((c) => healthyCounts.push(c), {
      minGapMs: 10,
    });
    transientFailWatchId = failingWatch.id;

    // Trigger failures on failingWatch so its backoff grows (50ms -> 100ms -> 200ms -> 400ms)!
    engine.create('Task', { title: 't1', done: false }, 't-1');
    advanceAfter(10); // failing fails (50ms backoff), healthy runs at +10ms
    expect(healthyCounts).toEqual([0, 1]);
    expect(failingErrors).toHaveLength(1);

    // Advance 50ms -> failing fails again (100ms backoff); advance 100ms -> fails again (200ms backoff)
    advanceAfter(50);
    expect(failingErrors).toHaveLength(2);
    advanceAfter(100);
    expect(failingErrors).toHaveLength(3);

    // Now failingWatch is in 200ms backoff! Mutate a Task -> healthyWatch has minGapMs: 10 and MUST fire
    // in 10ms, NOT wait 200ms behind failingWatch!
    engine.create('Task', { title: 't2', done: false }, 't-2');
    advanceAfter(15);
    expect(healthyCounts).toEqual([0, 1, 2]);

    // P1-16 (b): Turn-slicing (8ms) is respected even while failingWatch is backed off!
    const healthy2Counts: number[] = [];
    const healthy2 = engine.query.of('Task').watchCount((c) => healthy2Counts.push(c), {
      minGapMs: 0,
      adaptiveMultiplier: 0,
    });
    const healthy3Counts: number[] = [];
    const healthy3 = engine.query.of('Task').watchCount((c) => healthy3Counts.push(c), {
      minGapMs: 0,
      adaptiveMultiplier: 0,
    });
    // Make each rerun advance fake clock by 10ms (> 8ms turn slice!)
    rerunCostStepMs = 10;
    engine.create('Task', { title: 't3', done: false }, 't-3');
    // Fire exactly ONE scheduled timer turn — at most 1 healthy watch runs before yielding the 8ms slice!
    const firstTimerEntry = [...timers.entries()].sort((a, b) => a[1].at - b[1].at)[0]!;
    timers.delete(firstTimerEntry[0]);
    nowMs = Math.max(nowMs, firstTimerEntry[1].at);
    firstTimerEntry[1].run();
    const ranInSingleTurn =
      (healthyCounts.includes(3) ? 1 : 0)
      + (healthy2Counts.includes(3) ? 1 : 0)
      + (healthy3Counts.includes(3) ? 1 : 0);
    expect(ranInSingleTurn).toBe(1);
    // And a continuation timer was armed so remaining healthy watch completes on the next tick!
    expect(timers.size).toBe(1);
    rerunCostStepMs = 0;

    // Clear transient fault and verify onSettled() fires once everything settles!
    transientFailWatchId = undefined;
    let settledCallbacks = 0;
    const cancelNever = engine.watches.onSettled(() => {
      settledCallbacks += 100;
    });
    cancelNever();
    engine.watches.onSettled(() => {
      settledCallbacks++;
    });
    expect(settledCallbacks).toBe(0);
    advanceAfter(500);
    expect(engine.watches.settled()).toBe(true);
    expect(settledCallbacks).toBe(1);
    expect(healthy2Counts[healthy2Counts.length - 1]).toBe(3);
    expect(healthy3Counts[healthy3Counts.length - 1]).toBe(3);

    failingWatch.stop();
    healthyWatch.stop();
    healthy2.stop();
    healthy3.stop();

    // Review regressions (P1-1, P1-3, P2-1, P2-2):
    // 1. Invalid watchTurnSliceMs throws INVALID_ARGUMENT
    for (const badSlice of [Number.NaN, Number.POSITIVE_INFINITY, 0, -5, 1.5]) {
      expect(() =>
        createEarsEngine({ database: handle.database, clock: afterClock, watchTurnSliceMs: badSlice }),
      ).toThrowError(expect.objectContaining({ code: 'INVALID_ARGUMENT' }));
    }

    // 2. Stopping a failing watch inside onError fires onSettled AFTER onError (not mid-drain),
    //    leaves zero orphan timers, and duplicate onSettled registrations unsubscribe independently!
    const order: string[] = [];
    const steady = engine.query.of('Task').watchCount(() => {}, { minGapMs: 0, adaptiveMultiplier: 0 });
    const selfStopping = engine.query.of('Task').watchCount(() => {}, {
      minGapMs: 0,
      adaptiveMultiplier: 0,
      onError: () => {
        order.push('onError');
        selfStopping.stop();
      },
    });
    transientFailWatchId = selfStopping.id;
    engine.create('Task', { title: 't4', done: false }, 't-4');
    const sharedListener = (): void => {
      order.push('onSettled');
    };
    const cancelFirst = engine.watches.onSettled(sharedListener);
    engine.watches.onSettled(sharedListener);
    cancelFirst(); // Must NOT cancel the second registration of `sharedListener`!
    advanceAfter(20);
    expect(order).toEqual(['onError', 'onSettled']);
    expect(timers.size).toBe(0);
    steady.stop();
  });

  it('fail-stops and settles all watches on handle-fatal PANICKED/POISONED/DURABILITY_FAILED and pump-level errors (E2)', () => {
    // 1. Realistic latched native handle: once a panic/poison/durability fault occurs inside rerunWatch,
    //    EVERY subsequent native method (watchesSettled, tickWatches, unregisterWatch, etc.) throws too!
    for (const fatalCode of ['PANICKED', 'POISONED', 'DURABILITY_FAILED'] as const) {
      const clock = createFakeClock(20_000);
      const handle = temps.open({ schema: watchSchema });
      const realDb = handle.database;
      let triggerOnRerun = false;
      let handleLatched = false;
      const wrappedDb = new Proxy(realDb, {
        get(target, prop, receiver) {
          if (prop === 'rerunWatch') {
            return (watchId: number, spec: unknown, now: number, costMs: number) => {
              if (triggerOnRerun) {
                handleLatched = true;
                throw new EarsError(fatalCode, `simulated ${fatalCode} fault`);
              }
              return target.rerunWatch(watchId, spec as Parameters<typeof target.rerunWatch>[1], now, costMs);
            };
          }
          const value = Reflect.get(target, prop, receiver);
          if (typeof value !== 'function') return value;
          return (...args: unknown[]) => {
            if (handleLatched) {
              throw new EarsError(fatalCode, `latched ${fatalCode} handle`);
            }
            return (value as (...a: unknown[]) => unknown).apply(target, args);
          };
        },
      });

      const engine = createEarsEngine({ database: wrappedDb, clock });
      const w1Errors: EarsError[] = [];
      const w2Errors: EarsError[] = [];
      const w1 = engine.query.of('Task').watchCount(() => {}, {
        minGapMs: 0,
        onError: (err) => w1Errors.push(err),
      });
      const w2 = engine.query.of('Other').watchCount(() => {}, {
        minGapMs: 0,
        onError: (err) => w2Errors.push(err),
      });

      let settledCalls = 0;
      triggerOnRerun = true;
      engine.create('Task', { title: 'trigger', done: false }, 'p-1');
      engine.watches.onSettled(() => {
        settledCalls++;
      });

      clock.advance(10);
      expect(w1Errors).toHaveLength(1);
      expect(w1Errors[0]!.code).toBe(fatalCode);
      expect(w2Errors).toHaveLength(1);
      expect(w2Errors[0]!.code).toBe(fatalCode);
      expect(w1.active).toBe(false);
      expect(w2.active).toBe(false);
      expect(engine.watches.activeCount).toBe(0);
      expect(engine.watches.settled()).toBe(true);
      expect(settledCalls).toBe(1);
      expect(clock.pendingTimers).toBe(0);

      // Advancing well past any backoff window must never re-invoke onError!
      clock.advance(60_000);
      expect(w1Errors).toHaveLength(1);
      expect(w2Errors).toHaveLength(1);
    }

    // 2. Handle-fatal PANICKED or CLOSED thrown directly from nextDueWatch (pump-level read)
    //    must also fail-stop, notify every active watch's onError, clear timers, and fire onSettled!
    for (const pumpCode of ['PANICKED', 'CLOSED'] as const) {
      const clock = createFakeClock(30_000);
      const handle = temps.open({ schema: watchSchema });
      const realDb = handle.database;
      let latchNextDue = false;
      const wrappedDb = new Proxy(realDb, {
        get(target, prop, receiver) {
          const value = Reflect.get(target, prop, receiver);
          if (typeof value !== 'function') return value;
          return (...args: unknown[]) => {
            if (latchNextDue && (prop === 'nextDueWatch' || prop === 'watchesSettled' || prop === 'tickWatches')) {
              throw new EarsError(pumpCode, `pump-level ${pumpCode}`);
            }
            return (value as (...a: unknown[]) => unknown).apply(target, args);
          };
        },
      });

      const engine = createEarsEngine({ database: wrappedDb, clock });
      const errs: EarsError[] = [];
      const w = engine.query.of('Task').watchCount(() => {}, {
        minGapMs: 0,
        onError: (err) => errs.push(err),
      });
      engine.create('Task', { title: 't', done: false }, 'pd-1');
      let settled = 0;
      engine.watches.onSettled(() => {
        settled++;
      });
      latchNextDue = true;
      clock.advance(10);
      expect(errs).toHaveLength(1);
      expect(errs[0]!.code).toBe(pumpCode);
      expect(w.active).toBe(false);
      expect(engine.watches.activeCount).toBe(0);
      expect(engine.watches.settled()).toBe(true);
      expect(settled).toBe(1);
      expect(clock.pendingTimers).toBe(0);
    }
  });

  it('seeds filterLocal baseline unconditionally when immediate: false so unrelated raw mutations never fire spuriously (F2)', () => {
    const clock = createFakeClock(40_000);
    const handle = temps.open({ schema: watchSchema });
    const engine = createEarsEngine({ database: handle.database, clock });

    engine.create('Task', { title: 'initial-done', done: true }, 'f2-init');
    const declDeliveries: Array<readonly string[]> = [];
    const wDecl = engine.query
      .of('Task')
      .filterLocal((r) => r.fields['done'] === true)
      .watch((rows) => declDeliveries.push(rows.map((r) => r.id)), {
        immediate: false,
        minGapMs: 0,
      });

    // 1. No immediate delivery on registration.
    expect(declDeliveries).toEqual([]);

    // 2. Unrelated raw Task mutation filtered OUT by filterLocal must NOT trigger a spurious delivery!
    engine.create('Task', { title: 'not-done', done: false }, 'f2-ignored');
    clock.advance(20);
    expect(declDeliveries).toEqual([]);

    // 3. Mutation that DOES change the filtered output delivers once.
    engine.create('Task', { title: 'second-done', done: true }, 'f2-match');
    clock.advance(20);
    expect(declDeliveries).toEqual([['f2-init', 'f2-match']]);

    // 4. Another filtered-out mutation still does not fire spuriously.
    engine.patch('f2-ignored', { title: 'still-not-done' });
    clock.advance(20);
    expect(declDeliveries).toEqual([['f2-init', 'f2-match']]);

    // 5. Contrast: imperative mode with immediate: false still delivers on invalidation.
    const impDeliveries: number[] = [];
    const wImp = engine.query
      .of('Task')
      .filterLocal((r) => r.fields['done'] === true)
      .watch((rows) => impDeliveries.push(rows.length), {
        immediate: false,
        mode: 'imperative',
        minGapMs: 0,
      });
    expect(impDeliveries).toEqual([]);
    engine.patch('f2-ignored', { title: 'imperative-trigger' });
    clock.advance(20);
    expect(impDeliveries).toEqual([2]);

    wDecl.stop();
    wImp.stop();
  });

  it('PR #268 Batch 3: #computeNextDelay & register watchesSettled route fatal/CLOSED errors and settle onSettled (3.3)', () => {
    for (const code of ['PANICKED', 'CLOSED'] as const) {
      // (a) tickWatches throwing inside #computeNextDelay (via notifyMutation)
      const clock = createFakeClock(50_000);
      const handle = temps.open({ schema: watchSchema });
      const rawDb = handle.database;
      let failTick = false;
      const wrappedDb = new Proxy(rawDb, {
        get(target, prop, receiver) {
          if (prop === 'tickWatches') {
            return (nowMs: number) => {
              if (failTick) throw new EarsError(code, `injected ${code} in tickWatches`);
              return target.tickWatches(nowMs);
            };
          }
          const value = Reflect.get(target, prop, receiver);
          return typeof value === 'function' ? (value as (...a: unknown[]) => unknown).bind(target) : value;
        },
      });
      const seenErrors: string[] = [];
      const engine = createEarsEngine({
        database: wrappedDb,
        clock,
        onWatchError: (e) => seenErrors.push(e.code),
      });
      const w = engine.query.of('Task').watchCount(() => {}, { minGapMs: 0 });
      // Arm an onSettled listener while a mutation is pending, then inject fatal tickWatches!
      let settledCount = 0;
      rawDb.executeWireBatch(
        (require(clientElmdbPath) as { buildWireBatch: (fn: (b: { create: (t: string, f: Record<string, unknown>) => void }) => void) => Uint8Array }).buildWireBatch(
          (b) => { b.create('Task', { title: 'dirty', done: false }); },
        ),
      );
      engine.watches.onSettled(() => {
        settledCount++;
      });
      expect(settledCount).toBe(0);
      failTick = true;
      // Creating via engine triggers notifyMutation -> #scheduleNext -> #computeNextDelay -> tickWatches throws!
      engine.create('Other', { name: 'trigger-notify' });
      expect(seenErrors).toEqual([code]);
      expect(w.active).toBe(false);
      expect(engine.watches.activeCount).toBe(0);
      expect(settledCount).toBe(1);
      expect(clock.pendingTimers).toBe(0);

      // (b) watchesSettled throwing inside register() post-callback check
      const handle2 = temps.open({ schema: watchSchema });
      const rawDb2 = handle2.database;
      let failSettled = false;
      const wrappedDb2 = new Proxy(rawDb2, {
        get(target, prop, receiver) {
          if (prop === 'watchesSettled') {
            return () => {
              if (failSettled) throw new EarsError(code, `injected ${code} in watchesSettled`);
              return target.watchesSettled();
            };
          }
          const value = Reflect.get(target, prop, receiver);
          return typeof value === 'function' ? (value as (...a: unknown[]) => unknown).bind(target) : value;
        },
      });
      const seenErrors2: string[] = [];
      const engine2 = createEarsEngine({
        database: wrappedDb2,
        clock: createFakeClock(60_000),
        onWatchError: (e) => seenErrors2.push(e.code),
      });
      const w2 = engine2.query.of('Task').watchCount(
        () => {
          failSettled = true;
        },
        { minGapMs: 0 },
      );
      expect(seenErrors2).toEqual([code]);
      expect(w2.active).toBe(false);
      expect(engine2.watches.activeCount).toBe(0);
      expect(engine2.watches.settled()).toBe(true);
    }
  });

  it('PR #268 Batch 3: throwing observer.start, finishRerun, and clock.now never wedge watch in running=true (3.4)', () => {
    // 1. Throwing observer.start & finishRerun (both ok and error paths) isolate errors and never wedge watch!
    const clock = createFakeClock(70_000);
    const handle = temps.open({ schema: watchSchema });
    const rawDb = handle.database;
    let throwObsStart = false;
    let throwObsFinish = false;
    let failRerunOnce = false;
    const wrappedDb = new Proxy(rawDb, {
      get(target, prop, receiver) {
        if (prop === 'rerunWatch') {
          return (id: number, spec: Parameters<typeof target.rerunWatch>[1], nowMs?: number, costMs?: number) => {
            if (failRerunOnce) {
              failRerunOnce = false;
              throw new EarsError('IO', 'transient rerun failure');
            }
            return target.rerunWatch(id, spec, nowMs, costMs);
          };
        }
        const value = Reflect.get(target, prop, receiver);
        return typeof value === 'function' ? (value as (...a: unknown[]) => unknown).bind(target) : value;
      },
    });
    const errors: string[] = [];
    const counts: number[] = [];
    const engine = createEarsEngine({
      database: wrappedDb,
      clock,
      onWatchError: (e) => errors.push(e.message),
      observer: {
        start(op) {
          if (op.watch && throwObsStart) {
            throw new Error('observer.start boom');
          }
          return () => {
            if (op.watch && throwObsFinish) {
              throw new Error('observer.finish boom');
            }
          };
        },
      },
    });
    const w = engine.query.of('Task').watchCount((c) => counts.push(c), { minGapMs: 0 });
    expect(counts).toEqual([0]);

    // Throw inside observer.start -> watch still reruns and delivers!
    throwObsStart = true;
    engine.create('Task', { title: 'after-start-throw', done: false });
    clock.advance(20);
    expect(counts).toEqual([0, 1]);
    expect(errors.some((m) => m.includes('observer.start boom'))).toBe(true);
    throwObsStart = false;

    // Throw inside observer.finish on transient rerunWatch error -> abortWatchRun MUST still run so watch recovers!
    failRerunOnce = true;
    throwObsFinish = true;
    engine.create('Task', { title: 'after-finish-throw', done: false });
    clock.advance(20);
    expect(counts).toEqual([0, 1]);
    expect(errors.some((m) => m.includes('observer.finish boom'))).toBe(true);
    throwObsFinish = false;
    clock.advance(100);
    expect(counts).toEqual([0, 1, 2]);
    expect(engine.watches.settled()).toBe(true);
    w.stop();

    // 2. Throwing clock.now() right after nextDueWatch() calls abortWatchRun so watch never wedges!
    const baseClock = createFakeClock(80_000);
    let armClockThrowOnce = true;
    let throwOnNextNow = false;
    const flakyClock: EarsClock = {
      now() {
        if (throwOnNextNow) {
          throwOnNextNow = false;
          throw new Error('clock.now boom');
        }
        return baseClock.now();
      },
      setTimeout: (cb, ms) => baseClock.setTimeout(cb, ms),
      clearTimeout: (h) => baseClock.clearTimeout(h),
    };
    const handle2 = temps.open({ schema: watchSchema });
    const rawDb2 = handle2.database;
    let abortCalls = 0;
    const wrappedDb2 = new Proxy(rawDb2, {
      get(target, prop, receiver) {
        if (prop === 'nextDueWatch') {
          return (nowMs: number, elapsedMs: number) => {
            const res = target.nextDueWatch(nowMs, elapsedMs);
            if (armClockThrowOnce && res.watchId !== undefined) {
              armClockThrowOnce = false;
              throwOnNextNow = true;
            }
            return res;
          };
        }
        if (prop === 'abortWatchRun') {
          return (watchId: number, requeueDirty: boolean, nowMs?: number) => {
            abortCalls++;
            return target.abortWatchRun(watchId, requeueDirty, nowMs);
          };
        }
        const value = Reflect.get(target, prop, receiver);
        return typeof value === 'function' ? (value as (...a: unknown[]) => unknown).bind(target) : value;
      },
    });
    const clockErrors: string[] = [];
    const counts2: number[] = [];
    const engine2 = createEarsEngine({
      database: wrappedDb2,
      clock: flakyClock,
      onWatchError: (e) => clockErrors.push(e.message),
    });
    const w2 = engine2.query.of('Task').watchCount((c) => counts2.push(c), { minGapMs: 0, adaptiveMultiplier: 0 });
    expect(counts2).toEqual([0]);
    engine2.create('Task', { title: 'after-clock-throw', done: false });
    // Run flushSync once: nextDueWatch succeeds, then clock.now() at runStart throws -> abortWatchRun runs!
    baseClock.advance(10);
    engine2.watches.flushSync(baseClock.now());
    expect(abortCalls).toBe(1);
    expect(clockErrors.some((m) => m.includes('clock.now boom'))).toBe(true);
    // Advance past backoff and flush again -> watch is NOT wedged and delivers count = 1!
    baseClock.advance(200);
    engine2.watches.flushSync(baseClock.now());
    expect(counts2).toEqual([0, 1]);
    expect(engine2.watches.settled()).toBe(true);
    w2.stop();
  });

  it('PR #268 Batch 3: createSystemClock is monotonic integer ms and fractional clocks never kill watches (3.6)', async () => {
    const sys = createSystemClock();
    const t1 = sys.now();
    const t2 = sys.now();
    expect(Number.isSafeInteger(t1)).toBe(true);
    expect(Number.isSafeInteger(t2)).toBe(true);
    expect(t2).toBeGreaterThanOrEqual(t1);

    // Fractional clock (like raw `performance.now()`) MUST NOT cause INVALID_ARGUMENT or kill watches!
    const handle = temps.open({ schema: watchSchema });
    const base = createFakeClock(1000);
    const fractionalClock: EarsClock = {
      now: () => base.now() + 0.75,
      setTimeout: (cb, ms) => base.setTimeout(cb, ms),
      clearTimeout: (h) => base.clearTimeout(h),
    };
    const errors: EarsError[] = [];
    const counts: number[] = [];
    const engine = createEarsEngine({
      database: handle.database,
      clock: fractionalClock,
      onWatchError: (e) => errors.push(e),
    });
    const w = engine.query.of('Task').watchCount((c) => counts.push(c), { minGapMs: 0 });
    expect(counts).toEqual([0]);
    engine.create('Task', { title: 'frac', done: false });
    base.advance(20);
    expect(counts).toEqual([0, 1]);
    expect(errors).toEqual([]);
    expect(w.active).toBe(true);
    expect(engine.watches.settled()).toBe(true);
    // Out-of-safe-integer `flushSync(1e20)` rejects cleanly with INVALID_ARGUMENT instead of silently swallowing.
    expect(() => engine.watches.flushSync(1e20)).toThrowError(expect.objectContaining({ code: 'INVALID_ARGUMENT' }));
    w.stop();

    // Throwing `observer.finish` during `register()` unregisters the Rust watch so `settled()` never wedges false!
    const handle3 = temps.open({ schema: watchSchema });
    const eng3 = createEarsEngine({
      database: handle3.database,
      clock: createFakeClock(90_000),
      observer: {
        start(op) {
          return () => {
            if (op.watch) throw new Error('register finish boom');
          };
        },
      },
    });
    expect(() => eng3.query.of('Task').watchCount(() => {}, { minGapMs: 0 })).toThrow('register finish boom');
    expect(eng3.watches.activeCount).toBe(0);
    eng3.create('Task', { title: 'after-failed-register', done: false });
    expect(eng3.watches.settled()).toBe(true);

    // Throwing `clock.now()` when a scheduled timer fires does NOT crash or strand dirty watches!
    const base4 = createFakeClock(95_000);
    let failTimerNowOnce = false;
    const timerClock: EarsClock = {
      now() {
        if (failTimerNowOnce) {
          failTimerNowOnce = false;
          throw new Error('timer fire clock boom');
        }
        return base4.now();
      },
      setTimeout: (cb, ms) => base4.setTimeout(cb, ms),
      clearTimeout: (h) => base4.clearTimeout(h),
    };
    const handle4 = temps.open({ schema: watchSchema });
    const timerErrors: string[] = [];
    const timerCounts: number[] = [];
    const eng4 = createEarsEngine({
      database: handle4.database,
      clock: timerClock,
      onWatchError: (e) => timerErrors.push(e.message),
    });
    const w4 = eng4.query.of('Task').watchCount((c) => timerCounts.push(c), { minGapMs: 0, adaptiveMultiplier: 0 });
    eng4.create('Task', { title: 'timer-resilient', done: false });
    failTimerNowOnce = true;
    base4.advance(10);
    expect(timerErrors.some((m) => m.includes('timer fire clock boom'))).toBe(true);
    expect(timerCounts).toEqual([0, 1]);
    expect(eng4.watches.settled()).toBe(true);
    w4.stop();

    // Cross-batch P2: if a sibling watch is dirtied during `registerWatch` (by a real cross-process
    // `setSchema` bump) and the new watch's `observer.finish` throws, the sibling watch MUST still be
    // scheduled and rerun cleanly without requiring a manual `flushSync()`!
    const handle5 = temps.open({ schema: watchSchema });
    const clock5 = createFakeClock(100_000);
    let failSecondRegisterFinish = false;
    const siblingCounts: number[] = [];
    const eng5 = createEarsEngine({
      database: handle5.database,
      clock: clock5,
      observer: {
        start(op) {
          return () => {
            if (op.watch && failSecondRegisterFinish) {
              failSecondRegisterFinish = false;
              throw new Error('second register finish boom');
            }
          };
        },
      },
    });
    const siblingWatch = eng5.query.of('Task').watchCount((c) => siblingCounts.push(c), {
      minGapMs: 0,
      adaptiveMultiplier: 0,
    });
    expect(siblingCounts).toEqual([0]);

    // Child process bumps schema and inserts a row without eng5 calling catchUpCrossProcess.
    execFileSync(
      process.execPath,
      [
        '-e',
        `const { Database } = require(${JSON.stringify(nativeElmdbPath)});
         const { buildWireBatch } = require(${JSON.stringify(clientElmdbPath)});
         const db = Database.open(${JSON.stringify(handle5.path)}, { durability: 'strict' });
         try {
           db.setSchema({
             types: {
               Task: {
                 fields: {
                   title: { type: 'string' },
                   done: { type: 'boolean' },
                   priority: { type: 'integer', optional: true },
                 },
                 indexes: { byDone: { kind: 'value', fields: ['done'] } },
               },
               Other: { fields: { name: { type: 'string' } }, indexes: {} },
             },
             relations: {},
           });
           db.executeWireBatch(buildWireBatch((b) => { b.create('Task', { title: 'from-peer', done: false }, 'peer-t1'); }));
         } finally {
           db.close();
         }`,
      ],
      { stdio: 'inherit' },
    );

    // Registering a new watch on eng5 discovers the peer schema bump (dirtying siblingWatch),
    // then throws in `observer.finish`. The new watch is unregistered, AND siblingWatch is scheduled!
    failSecondRegisterFinish = true;
    expect(() => eng5.query.of('Task').watchCount(() => {}, { minGapMs: 0 })).toThrow('second register finish boom');
    expect(eng5.watches.activeCount).toBe(1);
    clock5.advance(10);
    expect(siblingCounts).toEqual([0, 1]);
    expect(eng5.watches.settled()).toBe(true);
    siblingWatch.stop();

    // PR #272 review fixes:
    // (a) Throwing custom clock `clearTimeout` during `register()` cleanup must NOT mask `observer.finish()`'s
    //     error and must still clear active watches & settle `onSettled()`.
    const handle6 = temps.open({ schema: watchSchema });
    const base6 = createFakeClock(110_000);
    let throwOnClearTimeout = false;
    let failSettledFatal = false;
    const hostileClearClock: EarsClock = {
      now: () => base6.now(),
      setTimeout: (cb, ms) => base6.setTimeout(cb, ms),
      clearTimeout(h) {
        base6.clearTimeout(h);
        if (throwOnClearTimeout) throw new Error('clock clearTimeout boom');
      },
    };
    const wrappedDb6 = new Proxy(handle6.database, {
      get(target, prop, receiver) {
        if (prop === 'watchesSettled') {
          return () => {
            if (failSettledFatal) throw new EarsError('PANICKED', 'simulated handle panic');
            return target.watchesSettled();
          };
        }
        const value = Reflect.get(target, prop, receiver);
        return typeof value === 'function' ? (value as (...a: unknown[]) => unknown).bind(target) : value;
      },
    });
    let closeAndThrowInFinish = false;
    let settledResolved = false;
    const eng6 = createEarsEngine({
      database: wrappedDb6,
      clock: hostileClearClock,
      observer: {
        start(op) {
          return () => {
            if (op.watch && closeAndThrowInFinish) {
              closeAndThrowInFinish = false;
              failSettledFatal = true;
              throw new Error('original finish error');
            }
          };
        },
      },
    });
    const w6 = eng6.query.of('Task').watchCount(() => {}, { minGapMs: 500, adaptiveMultiplier: 0 });
    eng6.create('Task', { title: 'arm-timer', done: false }); // arms timer on w6
    const unsubSettled6 = eng6.watches.onSettled(() => {
      settledResolved = true;
    });
    throwOnClearTimeout = true;
    closeAndThrowInFinish = true;
    expect(() => eng6.query.of('Task').watchCount(() => {}, { minGapMs: 0 })).toThrow('original finish error');
    expect(w6.active).toBe(false);
    expect(eng6.watches.activeCount).toBe(0);
    expect(eng6.watches.settled()).toBe(true);
    expect(settledResolved).toBe(true);
    unsubSettled6();

    // (b) Hostile getter on `options.onError` / `options.immediate` fails BEFORE registering in Rust (no leak).
    const handle7 = temps.open({ schema: watchSchema });
    const eng7 = createEarsEngine({ database: handle7.database, clock: createFakeClock(120_000) });
    const hostileOpts = {
      get onError() {
        throw new Error('hostile onError getter');
      },
    };
    expect(() => eng7.query.of('Task').watchCount(() => {}, hostileOpts as never)).toThrow('hostile onError getter');
    expect(eng7.watches.activeCount).toBe(0);
    eng7.create('Task', { title: 'no-leak-7', done: false });
    expect(eng7.watches.settled()).toBe(true);
  });
});
