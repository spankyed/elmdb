import { afterEach, describe, expect, it } from 'vitest';
import { createEarsEngine, type EarsClock, type EarsError, type Schema } from '../src/index.ts';
import { createTemporaryDatabases, openDatabase } from '../src/lmdb/index.ts';

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

    // Cross-handle commit (simulating cross-process write)
    const writer2 = openDatabase({ path: handle.path, durability: 'strict' });
    try {
      const e2 = createEarsEngine({ database: writer2 });
      e2.create('Task', { title: 'From other process', done: false }, 't2');
    } finally {
      writer2.close();
    }

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
});
