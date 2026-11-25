// Injectable clock contract: when `EARSRuntimeDeps.clock` is provided, every wall timestamp, generated id prefix,
// call duration, recency query, watch gap, and slice-deferral yield advances strictly on virtual clock ticks.
import { afterAll, describe, expect, it } from 'vitest';
import { createEarsEngine, type EARS, type EarsClock, type EarsDatabase, type WatchSchedule } from '../../src/index.ts';
import { newId } from '../../src/ids.ts';
import { createTemporaryDatabases } from '../../src/lmdb/index.ts';

const databases = createTemporaryDatabases();
afterAll(() => databases.disposeAll());

/** Deterministic manual clock matching the structural `EarsClock` contract */
function createVirtualClock(initialMs = 1_700_000_000_000) {
  let current = initialMs;
  let nextId = 0;
  const timers = new Map<number, { at: number; order: number; run: () => void }>();

  const clock: EarsClock = {
    now: () => current,
    after(ms: number, run: () => void): () => void {
      const id = ++nextId;
      const delay = Math.max(0, ms);
      timers.set(id, { at: current + delay, order: id, run });
      return () => { timers.delete(id); };
    },
  };

  const nextDue = () => {
    let best: { id: number; at: number; order: number; run: () => void } | undefined;
    for (const [id, entry] of timers) {
      if (entry.at > current) continue;
      if (!best || entry.at < best.at || (entry.at === best.at && entry.order < best.order)) {
        best = { id, ...entry };
      }
    }
    return best;
  };

  return {
    clock,
    now: () => current,
    pending: () => timers.size,
    /** Advances virtual time without firing timers, simulating synchronous work inside a call or watch read */
    spend(ms: number) {
      current += ms;
    },
    /** Advances virtual time by `ms` and fires every timer due at or before the new time in deadline order */
    advance(ms: number) {
      const target = current + ms;
      while (true) {
        let earliest: { id: number; at: number; order: number; run: () => void } | undefined;
        for (const [id, entry] of timers) {
          if (entry.at > target) continue;
          if (!earliest || entry.at < earliest.at || (entry.at === earliest.at && entry.order < earliest.order)) {
            earliest = { id, ...entry };
          }
        }
        if (!earliest) break;
        current = Math.max(current, earliest.at);
        timers.delete(earliest.id);
        earliest.run();
      }
      current = Math.max(current, target);
    },
    /** Fires timers already due at the current virtual timestamp (including `after(0, fn)` yields) */
    flushDue() {
      let due = nextDue();
      while (due) {
        timers.delete(due.id);
        due.run();
        due = nextDue();
      }
    },
  };
}

const flushMicrotasks = async () => {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
};

const isResolved = async (promise: Promise<void>) => {
  let done = false;
  void promise.then(() => { done = true; });
  await flushMicrotasks();
  return done;
};

describe('injectable EarsClock', () => {
  it('drives entity timestamps, WriteBuilder chains, trash, upsert, and generated id prefixes', () => {
    const start = 1_700_000_000_000;
    const virtual = createVirtualClock(start);
    const { database } = databases.open({
      types: { Task: { required: [] } },
      relations: { blocks: {} },
      roles: [],
    });
    const engine = createEarsEngine({ database, clock: virtual.clock });

    // Entity creation & generated entity id prefix
    const createdId = engine.create('Task', { title: 'first' }).id();
    expect(createdId.startsWith(`Task-${start.toString(36)}`)).toBe(true);
    expect(engine.getRow(createdId)).toMatchObject({ createdAt: start, updatedAt: start });

    // Standalone `newId` with optional `now` provider
    expect(newId('Task', virtual.clock.now).startsWith(`Task-${start.toString(36)}`)).toBe(true);

    // Explicit caller-provided timestamps remain respected
    const importedId = engine.create('Task', { title: 'imported', createdAt: 1234, updatedAt: 5678 }).id();
    expect(engine.getRow(importedId)).toMatchObject({ createdAt: 1234, updatedAt: 5678 });

    // WriteBuilder stamps updatedAt from virtual clock and reuses the first write's timestamp across one chain
    virtual.advance(5_000);
    const builder = engine.write(createdId);
    builder.set('title', 'second');
    virtual.advance(2_000);
    builder.set('status', 'done');
    expect(engine.getRow(createdId)).toMatchObject({
      createdAt: start,
      updatedAt: start + 5_000,
      title: 'second',
      status: 'done',
    });

    // `touch: false` does not stamp updatedAt
    virtual.advance(3_000);
    engine.write(createdId, { touch: false }).set('status', 'archived');
    expect(engine.getRow(createdId)).toMatchObject({ updatedAt: start + 5_000 });

    // `trash()` uses virtual clock by default and respects explicit `at`
    virtual.advance(4_000);
    engine.write(createdId).trash();
    expect(engine.getRow(createdId)?.trashedAt).toBe(start + 14_000);
    engine.write(importedId).trash(99_999);
    expect(engine.getRow(importedId)?.trashedAt).toBe(99_999);
    engine.write(createdId).restore();
    engine.write(importedId).restore();

    // `upsert()` creating and updating both use virtual clock
    virtual.advance(6_000);
    const upsertedId = 'Task-upserted' as EARS.EntityId;
    engine.upsert('Task', { id: upsertedId, title: 'v1' });
    expect(engine.getRow(upsertedId)).toMatchObject({ createdAt: start + 20_000, updatedAt: start + 20_000 });

    virtual.advance(7_000);
    const upsertBuilder = engine.upsert('Task', { id: upsertedId, title: 'v2' });
    virtual.advance(1_000);
    upsertBuilder.set('extra', 'same-stamp');
    expect(engine.getRow(upsertedId)).toMatchObject({
      createdAt: start + 20_000,
      updatedAt: start + 27_000,
      title: 'v2',
      extra: 'same-stamp',
    });

    // Relation IDs (both `link` via Rust Batch.now and `admin.addRelation` via JS newRelationId) carry clock prefix
    virtual.advance(10_000);
    const linkStamp = virtual.now().toString(36);
    engine.write(createdId).link('blocks', upsertedId);
    const [linked] = engine.findRelations({ source: createdId, kind: 'blocks' });
    expect(linked!.id.startsWith(`Relation-${linkStamp}`)).toBe(true);

    virtual.advance(10_000);
    const adminRelStamp = virtual.now().toString(36);
    const adminRelId = engine.admin.addRelation(upsertedId, 'blocks', createdId);
    expect(adminRelId.startsWith(`Relation-${adminRelStamp}`)).toBe(true);
  });

  it('drives admin.calls durations strictly from the injected clock', () => {
    const virtual = createVirtualClock(10_000);
    const { database } = databases.open({
      types: { Task: { required: [] } },
      relations: {},
      roles: [],
    });
    // Advance virtual clock deterministically inside database read/write/commit crossings
    const timedDb: EarsDatabase = new Proxy(database, {
      get(target, key) {
        if (key === 'read') {
          return (reads: Uint8Array) => {
            virtual.spend(12);
            return target.read(reads);
          };
        }
        if (key === 'write') {
          return (batch: Uint8Array, durable?: boolean | null) => {
            virtual.spend(17);
            return target.write(batch, durable);
          };
        }
        if (key === 'begin') {
          return () => {
            const unit = target.begin();
            return new Proxy(unit, {
              get(held, member) {
                if (member === 'write') {
                  return (batch: Uint8Array) => {
                    virtual.spend(9);
                    return held.write(batch);
                  };
                }
                if (member === 'commit') {
                  return (durable?: boolean | null) => {
                    virtual.spend(4);
                    return held.commit(durable);
                  };
                }
                const value = Reflect.get(held, member, held);
                return typeof value === 'function' ? value.bind(held) : value;
              },
            });
          };
        }
        const value = Reflect.get(target, key, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });

    const engine = createEarsEngine({ database: timedDb, clock: virtual.clock });
    engine.admin.calls.start();
    engine.create('Task', { title: 'one' });
    engine.query('Task').ids();
    engine.transact(() => {
      engine.create('Task', { title: 'two' });
    });
    virtual.spend(50);
    const report = engine.admin.calls.stop();

    expect(report.totals.write).toEqual({ calls: 1, ops: 1, ms: 17 });
    expect(report.totals.read).toEqual({ calls: 1, ops: 1, ms: 12 });
    expect(report.totals.unit).toEqual({ calls: 1, ops: 1, ms: 13 });
    expect(report.elapsedMs).toBe(17 + 12 + 13 + 50);
  });

  it('populates omitted search and hybrid recency.now from the injected clock at read time', () => {
    const start = 1_000_000;
    const virtual = createVirtualClock(start);
    const { database } = databases.open({
      types: {
        Note: {
          required: [],
          search: { fields: { title: 1 } },
          embed: { from: ['title'], model: 'test:embed', dims: 2 },
        },
      },
      relations: {},
      roles: [],
    });
    const engine = createEarsEngine({ database, clock: virtual.clock });

    const older = engine.create('Note', { title: 'deploy release' }).id();
    virtual.advance(10_000);
    const newer = engine.create('Note', { title: 'deploy release' }).id();

    // Embed both with identical vectors so hybrid text recency determines ranking
    const pending = engine.pendingEmbeddings('Note');
    engine.embed(pending.chunks.map((chunk) => ({ ...chunk, model: pending.model, vector: [1, 0] })));

    // Keep one search recipe and evaluate it at two virtual timestamps (10_000 ms apart = one half-life)
    const searchQuery = engine.query('Note').search('deploy', {
      recency: { field: 'createdAt', halfLife: 10_000 },
    });
    const hitsAtNewer = searchQuery.hits();
    expect(hitsAtNewer.map((h) => h.id)).toEqual([newer, older]);
    const newerScore0 = hitsAtNewer[0]!.score;
    const olderScore0 = hitsAtNewer[1]!.score;
    // Older is exactly 1 half-life behind newer, so its score is half of newer's
    expect(olderScore0 / newerScore0).toBeCloseTo(0.5, 5);

    // Advance clock by one half-life: reusing the same recipe halves both scores without rebuilding the query
    virtual.advance(10_000);
    const hitsLater = searchQuery.hits();
    expect(hitsLater[0]!.score / newerScore0).toBeCloseTo(0.5, 5);
    expect(hitsLater[1]!.score / olderScore0).toBeCloseTo(0.5, 5);

    // Explicit `recency.now` is never overwritten by the injected clock
    const pinnedHits = engine.query('Note').search('deploy', {
      recency: { field: 'createdAt', halfLife: 10_000, now: start + 10_000 },
    }).hits();
    expect(pinnedHits[0]!.score).toBeCloseTo(newerScore0, 5);

    // `hybrid` without explicit `recency.now` also uses the virtual clock (and stays deterministic at future times)
    const hybridQuery = engine.query('Note').hybrid('deploy', [1, 0], {
      recency: { field: 'createdAt', halfLife: 10_000 },
    });
    const hybridHits = hybridQuery.hits();
    expect(hybridHits.map((h) => h.id)).toEqual([newer, older]);
    // Verify `explain()` succeeds with recency filled from virtual clock
    expect(searchQuery.explain().ids).toEqual([newer, older]);
  });

  it('routes watch gaps, custom schedule, later() slice yields, caughtUp, and watchesSettled strictly through EarsClock', async () => {
    const virtual = createVirtualClock(50_000);
    const schedule: WatchSchedule = { delayFactor: 4, minimumGapMs: 25, flushSliceMs: 8 };
    const { database } = databases.open({
      types: { Task: { required: [] } },
      relations: {},
      roles: [],
    });
    const engine = createEarsEngine({ database, clock: virtual.clock, schedule });

    const order: string[] = [];
    const seenA: number[] = [];
    const seenB: number[] = [];
    const wA = engine.watch(() => {
      order.push('a');
      virtual.spend(5);
      return engine.query('Task').count();
    }, (count) => { seenA.push(count); }, { label: 'a' });
    const wB = engine.watch(() => {
      order.push('b');
      virtual.spend(5);
      return engine.query('Task').count();
    }, (count) => { seenB.push(count); }, { label: 'b' });

    // Initial flush runs both watches (b's costMs is 0 before its first run, so 5 + 0 <= 8 ms slice)
    await flushMicrotasks();
    expect(order).toEqual(['a', 'b']);
    expect(seenA).toEqual([0]);
    expect(seenB).toEqual([0]);
    expect(await isResolved(engine.admin.watchesSettled())).toBe(true);

    // Advance well past both watches' gaps so both are idle, then commit once:
    // now `a` runs (5 ms), and `b` (known costMs = 5 ms) would overrun the 8 ms slice and defers via clock.after(0)
    virtual.advance(100);
    order.length = 0;
    engine.create('Task', { title: 't1' });
    await flushMicrotasks();
    expect(order).toEqual(['a']);
    expect(seenA).toEqual([0, 1]);
    expect(seenB).toEqual([0]);
    expect(virtual.pending(), 'slice deferral scheduled via clock.after(0)').toBe(1);

    // Waiting a real event loop tick does NOT run `b` — only flushing the virtual clock runs `b`
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(order, 'no real setImmediate leak').toEqual(['a']);

    // Flush the virtual clock's 0 ms timer: `b` now runs
    virtual.flushDue();
    expect(order).toEqual(['a', 'b']);
    expect(seenB).toEqual([0, 1]);
    expect(await isResolved(engine.admin.watchesSettled())).toBe(true);

    // Commit while `a` and `b` are inside their 25 ms custom minimumGapMs
    order.length = 0;
    engine.create('Task', { title: 't2' });
    const caughtA = wA.caughtUp();
    const settled = engine.admin.watchesSettled();
    await flushMicrotasks();
    expect(seenA).toEqual([0, 1]);
    expect(await isResolved(caughtA)).toBe(false);
    expect(await isResolved(settled)).toBe(false);
    expect(virtual.pending()).toBe(1);

    // `a` ran at t+105 (due at t+130); `b` ran at t+110 (due at t+135); current virtual time is t+110.
    // Advance 19 ms (to t+129) — `a` is still 1 ms inside its 25 ms custom gap
    virtual.advance(19);
    await flushMicrotasks();
    expect(seenA).toEqual([0, 1]);
    expect(await isResolved(caughtA)).toBe(false);

    // Advance 1 ms (to t+130) — `a` becomes due and runs on microtask; `b` is still 5 ms behind
    virtual.advance(1);
    await flushMicrotasks();
    expect(seenA).toEqual([0, 1, 2]);
    expect(await isResolved(caughtA)).toBe(true);
    expect(seenB).toEqual([0, 1]);
    expect(await isResolved(settled)).toBe(false);

    // Advance remaining 5 ms (from t+135 -> `b` is already due at t+135, wait: `a` spent 5 ms so now is t+135!)
    // Actually `a`'s run took 5 ms (t+130..t+135), so `b` (due at t+135) is now due and deferred via clock.after(0)!
    expect(virtual.pending()).toBe(1);
    virtual.flushDue();
    await flushMicrotasks();
    expect(seenB).toEqual([0, 1, 2]);
    expect(await isResolved(settled)).toBe(true);

    // `caughtUp({ soon: true })` lifts the 25 ms floor down to 4 * 5 = 20 ms
    // `a` last ended at t+135 (cost 5 ms -> due at t+155 with soon), current time is t+140 (after `b` spent 5 ms)
    engine.create('Task', { title: 't3' });
    const soonA = wA.caughtUp({ soon: true });
    virtual.advance(14); // t+154: 19 ms since `a` ended (< 20 ms)
    await flushMicrotasks();
    expect(await isResolved(soonA)).toBe(false);
    virtual.advance(1); // t+155: 20 ms since `a` ended -> due!
    await flushMicrotasks();
    expect(await isResolved(soonA)).toBe(true);
    expect(seenA).toEqual([0, 1, 2, 3]);

    // Finish `b` and stop cleanly
    virtual.advance(25);
    virtual.flushDue();
    await flushMicrotasks();
    expect(await isResolved(engine.admin.watchesSettled())).toBe(true);
    wA.stop();
    wB.stop();
  });

  it('routes caughtUp and watchesSettled sequence-ahead yields through EarsClock', async () => {
    const virtual = createVirtualClock(100_000);
    const { database } = databases.open({
      types: { Task: { required: [] } },
      relations: {},
      roles: [],
    });
    let sequenceOffset = 0;
    const wrapped: EarsDatabase = new Proxy(database, {
      get(target, key) {
        if (key === 'sequence') return target.sequence + sequenceOffset;
        const value = Reflect.get(target, key, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });
    const engine = createEarsEngine({ database: wrapped, clock: virtual.clock });
    const w = engine.watch(() => engine.query('Task').count(), () => {});
    await flushMicrotasks();
    expect(await isResolved(w.caughtUp())).toBe(true);

    // Simulate database.sequence advancing ahead of lastSeenCommit (e.g. pending async builder notification)
    sequenceOffset = 1;
    const waitCaughtUp = w.caughtUp();
    const waitSettled = engine.admin.watchesSettled();
    await flushMicrotasks();
    expect(await isResolved(waitCaughtUp)).toBe(false);
    expect(await isResolved(waitSettled)).toBe(false);
    expect(virtual.pending(), 'both sequence-ahead yields scheduled on virtual clock').toBe(2);

    // Real event loop turn must NOT resolve them before the virtual clock flushes its 0 ms timers
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(await isResolved(waitCaughtUp)).toBe(false);
    expect(await isResolved(waitSettled)).toBe(false);

    // Flushing virtual clock 0 ms timers resolves both deterministically
    virtual.flushDue();
    await flushMicrotasks();
    expect(await isResolved(waitCaughtUp)).toBe(true);
    expect(await isResolved(waitSettled)).toBe(true);
    expect(virtual.pending()).toBe(0);

    // Stopping the watch cancels any pending sequence-ahead defer timers so none leak in the virtual clock
    sequenceOffset = 2;
    const stopCaughtUp = w.caughtUp();
    const stopSettled = engine.admin.watchesSettled();
    await flushMicrotasks();
    expect(virtual.pending()).toBe(2);
    w.stop();
    expect(virtual.pending(), 'stop() cancelled pending sequence-ahead defers').toBe(0);
    expect(await isResolved(stopCaughtUp)).toBe(true);
    expect(await isResolved(stopSettled)).toBe(true);

    // With 0 open watches, watchesSettled() resolves immediately without scheduling any virtual clock timer
    sequenceOffset = 3;
    expect(await isResolved(engine.admin.watchesSettled())).toBe(true);
    expect(virtual.pending()).toBe(0);

    // Closing the database cancels pending sequence-ahead defer timers as well
    const w2 = engine.watch(() => engine.query('Task').count(), () => {});
    await flushMicrotasks();
    sequenceOffset = 4;
    const closeCaughtUp = w2.caughtUp();
    const closeSettled = engine.admin.watchesSettled();
    await flushMicrotasks();
    expect(virtual.pending()).toBe(2);
    database.close();
    expect(await isResolved(w2.caughtUp())).toBe(true);
    expect(virtual.pending(), 'closed() cancelled pending sequence-ahead defers').toBe(0);
    expect(await isResolved(closeCaughtUp)).toBe(true);
    expect(await isResolved(closeSettled)).toBe(true);
  });

  it('cancels a pending slice-deferral timer when stop() clears the last dirty watch, and clamps negative watch costMs', async () => {
    let nowMs = 1_000;
    let nextTimer = 0;
    const timers = new Map<number, () => void>();
    const clock: EarsClock = {
      now: () => nowMs,
      after(_ms, run) {
        const id = ++nextTimer;
        timers.set(id, run);
        return () => { timers.delete(id); };
      },
    };
    const { database } = databases.open({
      types: { Task: { required: [] } },
      relations: {},
      roles: [],
    });
    const engine = createEarsEngine({
      database,
      clock,
      schedule: { delayFactor: 4, minimumGapMs: 16, flushSliceMs: 8 },
    });

    // Non-monotonic clock going backward during a watch read must clamp costMs to 0
    const backward = engine.watch(() => {
      nowMs -= 10;
      return engine.query('Task').count();
    }, () => {}, { label: 'backward' });
    await flushMicrotasks();
    expect(engine.admin.watchDiagnostics().watches.find((w) => w.label === 'backward')?.costMs).toBe(0);
    backward.stop();

    // Two 5 ms watches: on commit, `a` runs and `b` defers via clock.after(0); stopping `b` cancels that timer
    const a = engine.watch(() => { nowMs += 5; return engine.query('Task').count(); }, () => {}, { label: 'a' });
    const b = engine.watch(() => { nowMs += 5; return engine.query('Task').count(); }, () => {}, { label: 'b' });
    await flushMicrotasks();
    nowMs += 100;
    engine.create('Task', { title: 't' });
    await flushMicrotasks();
    expect(timers.size, 'b deferred via clock.after(0)').toBe(1);
    b.stop();
    expect(timers.size, 'stopping the last dirty watch cancelled its pending later flush timer').toBe(0);
    expect(await isResolved(engine.admin.watchesSettled())).toBe(true);
    a.stop();
  });

  it('keeps watch gaps and call durations on monotonic() when wall time now() jumps', async () => {
    let wallMs = 1_750_000_000_000;
    let monoMs = 500;
    const scheduled: Array<{ due: number; fn: () => void; cancelled: boolean }> = [];
    const clock: EarsClock = {
      now: () => wallMs,
      monotonic: () => monoMs,
      after(ms, run) {
        const entry = { due: monoMs + Math.max(0, ms), fn: run, cancelled: false };
        scheduled.push(entry);
        return () => { entry.cancelled = true; };
      },
    };
    const advanceMono = (ms: number) => {
      monoMs += ms;
      for (;;) {
        const due = scheduled
          .filter((entry) => !entry.cancelled && entry.due <= monoMs)
          .sort((a, b) => a.due - b.due)[0];
        if (!due) break;
        due.cancelled = true;
        due.fn();
      }
    };

    const { database } = databases.open({
      types: { Task: { required: [] } },
      relations: {},
      roles: [],
    });
    const engine = createEarsEngine({
      database,
      clock,
      schedule: { delayFactor: 4, minimumGapMs: 20, flushSliceMs: 8 },
    });

    const delivered: number[] = [];
    const watching = engine.watch(() => {
      monoMs += 2;
      return engine.query('Task').count();
    }, (count) => delivered.push(count), { label: 'mono' });
    await flushMicrotasks();
    expect(delivered).toEqual([0]);

    // Jump wall clock forward by 10 seconds while monotonic time does not move:
    // entity timestamps follow wall time, while the watch still respects its 20 ms monotonic gap
    wallMs += 10_000;
    engine.admin.calls.start();
    const id = engine.create('Task', { title: 'after-wall-jump' }).id();
    const report = engine.admin.calls.stop();
    expect(engine.getRow(id)?.createdAt).toBe(1_750_000_010_000);
    expect(report.elapsedMs).toBe(0);

    await flushMicrotasks();
    expect(delivered, 'wall-clock jump must not bypass monotonic watch gap').toEqual([0]);
    expect(engine.admin.watchDiagnostics().scheduler.timerAt).toBe(522);

    advanceMono(19);
    await flushMicrotasks();
    expect(delivered).toEqual([0]);

    advanceMono(1);
    await flushMicrotasks();
    expect(delivered).toEqual([0, 1]);
    watching.stop();
  });
});
