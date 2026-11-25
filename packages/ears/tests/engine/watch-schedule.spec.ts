// The watch scheduler's guarantees under any write rate (G14 of elmdb's goal-elmdb-next.md), on a clock the spec
// moves: a read's cost is what it advances the clock by, so a run is as cheap or as expensive as a case says, on any
// machine. Commits are real ones, to a real database; turns of the event loop are real too.
import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import fc from 'fast-check';
import { createConnection, type Connection } from '../../src/database.ts';
import { createWatches, WATCH_SCHEDULE, type WatchClock, type Watches } from '../../src/watch.ts';
import { createTemporaryDatabases } from '../../src/lmdb/index.ts';
import type { EarsWarning } from '../../src/index.ts';

const databases = createTemporaryDatabases();
afterAll(() => databases.disposeAll());

/** A clock that moves only when told, firing the timers it passes */
function fakeClock() {
  let now = 0;
  let next = 0;
  const timers = new Map<number, { at: number; fn: () => void; held: boolean }>();
  const clock: WatchClock = {
    now: () => now,
    setTimeout: (fn, ms, held = false) => {
      timers.set(++next, { at: now + ms, fn, held });
      return next;
    },
    clearTimeout: (handle) => { timers.delete(handle as number); },
  };
  const fireDue = () => {
    for (const [handle, timer] of [...timers].sort((a, b) => a[1].at - b[1].at)) {
      if (timer.at > now) continue;
      timers.delete(handle);
      timer.fn();
    }
  };
  return {
    clock,
    /** Moves the clock by `ms`, firing every timer due by then */
    advance(ms: number) {
      now += ms;
      fireDue();
    },
    /** What a read costs: the clock moves while it runs */
    spend(ms: number) { now += ms; },
    pending: () => timers.size,
    /** Whether each timer set is one something waits on */
    held: () => [...timers.values()].map((timer) => timer.held),
  };
}

/** A turn of the event loop: microtasks, then a later macrotask's work */
const turn = () => new Promise<void>((resolve) => setImmediate(resolve));

let connection: Connection;
let time: ReturnType<typeof fakeClock>;
let watches: Watches;
let warnings: string[];
let warned: EarsWarning[];
let sequence = 0;

/** A write outside a unit, to the one row every watch here reads */
const commit = () => {
  sequence++;
  connection.write([{ op: 'create', type: 'Row', id: `Row-${sequence}`, fields: { n: sequence } }]);
};
/** What every watch here reads: the rows, as ids */
const rows = () => (connection.read([{ read: 'query', plan: { scope: 'live', start: { from: 'types', types: ['Row'] }, steps: [], terminal: { answer: 'ids' } } }])[0]!.ids as string[]);

/** A watch of the rows whose every run costs `costMs`, counting its runs and keeping what it was handed */
function watching(costMs: number, label = `costs ${costMs} ms`) {
  const seen: string[][] = [];
  let runs = 0;
  const handle = watches.watch(() => {
    runs++;
    time.spend(costMs);
    return rows();
  }, (value) => { seen.push(value); }, { label });
  return { seen, runs: () => runs, last: () => seen.at(-1), stop: () => handle.stop(), caughtUp: (options?: { soon?: boolean }) => handle.caughtUp(options) };
}

/** Lets every pending run happen: turns and timers, until nothing waits */
async function drain() {
  for (let i = 0; i < 10_000 && !(await settledNow()); i++) {
    await turn();
    time.advance(WATCH_SCHEDULE.minimumGapMs);
  }
}
const settledNow = async () => {
  let done = false;
  void watches.settled().then(() => { done = true; });
  await Promise.resolve();
  await Promise.resolve();
  return done;
};

/** Whether a promise has resolved by now, without waiting a turn for it */
const resolved = async (promise: Promise<void>) => {
  let done = false;
  void promise.then(() => { done = true; });
  await Promise.resolve();
  await Promise.resolve();
  return done;
};

const fresh = () => {
  const { database } = databases.open({ types: { Row: { required: [] } }, relations: {}, roles: [] });
  connection = createConnection(database);
  time = fakeClock();
  warnings = [];
  warned = [];
  watches = createWatches(connection, { clock: time.clock, warn: (message, warning) => { warnings.push(message); warned.push(warning); } });
  sequence = 0;
};
beforeEach(fresh);

describe('the watch scheduler', () => {
  it('costs a bounded number of runs for a thousand commits on separate turns, and delivers the last result', async () => {
    const w = watching(0.05);
    await drain();
    const before = w.runs();
    for (let i = 0; i < 1000; i++) {
      commit();
      await turn();
      time.advance(1);
    }
    await drain();
    // A thousand commits over a thousand clock milliseconds: at most one run per gap, and the one that follows them
    expect(w.runs() - before).toBeLessThanOrEqual(1000 / WATCH_SCHEDULE.minimumGapMs + 2);
    expect(w.runs() - before).toBeGreaterThan(1);
    expect(w.last()).toEqual(rows());
    expect(w.last()).toHaveLength(1000);
  });

  it('runs a watch idle when a commit lands at once, on the clock it was committed at', async () => {
    const w = watching(1);
    await drain();
    time.advance(1000);
    commit();
    await Promise.resolve();
    await Promise.resolve();
    expect(w.last()).toEqual(rows());
    expect(time.pending(), 'no timer was needed').toBe(0);
  });

  it("doesn't hold a cheap watch to an expensive one's gap", async () => {
    const cheap = watching(0.1, 'cheap');
    const expensive = watching(100, 'expensive');
    await drain();
    time.advance(1000);
    // Both idle: both run for the first commit, the cheap one at most a turn after the expensive one used the slice
    commit();
    await turn();
    await turn();
    expect(cheap.last()).toHaveLength(1);
    expect(expensive.last()).toHaveLength(1);
    // Now the expensive one is in a gap of four times its run, and the cheap one hears each commit a frame apart
    for (let i = 0; i < 5; i++) {
      time.advance(WATCH_SCHEDULE.minimumGapMs);
      commit();
      await turn();
      await turn();
      expect(cheap.last(), `commit ${i + 2}`).toHaveLength(i + 2);
    }
    expect(expensive.last(), 'still in its gap').toHaveLength(1);
    await drain();
    expect(expensive.last()).toEqual(rows());
  });

  it('spends at most a slice of runs a flush, and runs those it left over first', async () => {
    const order: string[] = [];
    const ws = ['a', 'b', 'c', 'd'].map((label) => watches.watch(() => {
      order.push(label);
      time.spend(5);
      return rows();
    }, () => {}, { label }));
    await drain();
    time.advance(1000);
    order.length = 0;
    commit();
    await Promise.resolve();
    await Promise.resolve();
    // One 5 ms run fits an 8 ms slice; a second would overrun it
    expect(order).toEqual(['a']);
    await turn();
    expect(order).toEqual(['a', 'b']);
    await drain();
    expect(order).toEqual(['a', 'b', 'c', 'd']);
    for (const w of ws) w.stop();
  });

  it('settles once a watch due before its timer fired has run, the timer left to fire on nothing', async () => {
    const w = watching(1);
    await drain();
    commit();
    await turn();
    // In its gap: a timer stands for when it is due. The clock passes that without the timer firing, as a loaded
    // process's timers fire late, and the next commit finds it due and flushes it at once
    expect(time.pending()).toBe(1);
    time.spend(WATCH_SCHEDULE.minimumGapMs * 2);
    commit();
    expect(await Promise.race([watches.settled().then(() => 'settled'), turn().then(turn).then(() => 'waiting')])).toBe('settled');
    expect(w.last()).toEqual(rows());
  });

  it('always runs one watch a flush, however much its last run cost', async () => {
    const w = watching(50);
    await drain();
    time.advance(1000);
    commit();
    await Promise.resolve();
    await Promise.resolve();
    expect(w.last()).toHaveLength(1);
  });

  it('warns once, naming it, about a watch that keeps costing more than a slice', async () => {
    watching(20, 'the costly view');
    await drain();
    for (let i = 0; i < 6; i++) {
      commit();
      await drain();
    }
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain('the costly view');
    expect(warnings[0]).toContain('20.0 ms a run');
    expect(warned).toEqual([{ kind: 'slowWatch', label: 'the costly view', ms: 20, runs: expect.any(Number) }]);
  });

  it('counts each watch\'s runs and the states it skipped, while the calls are counted', async () => {
    const w = watching(30, 'counted');
    await drain();
    connection.calls.start();
    for (let i = 0; i < 5; i++) {
      commit();
      await turn();
    }
    await drain();
    const report = connection.calls.stop();
    const counted = report.watches.find((watch) => watch.label === 'counted')!;
    expect(counted.runs).toBeGreaterThan(0);
    expect(counted.runs + counted.skipped).toBeGreaterThanOrEqual(5);
    expect(counted.skipped).toBeGreaterThan(0);
    expect(report.totals.watchRuns.calls).toBe(counted.runs);
    expect(w.last()).toEqual(rows());
  });

  it("doesn't settle while a flush still holds a watch it hasn't run", async () => {
    let asked: Promise<void> | undefined;
    let asking = false;
    // The first watch's run fills most of the slice, so the second is left to a later flush; the first's observer
    // asks for settling between them
    watches.watch(() => { time.spend(6); return rows(); }, () => { if (asking) asked ??= watches.settled(); }, { label: 'asks' });
    const second = watching(5, 'second');
    await drain();
    asking = true;
    time.advance(1000);
    commit();
    await Promise.resolve();
    await Promise.resolve();
    expect(asked, 'the first watch ran and asked').toBeDefined();
    expect(second.last(), 'the second is still to run').toHaveLength(0);
    let settled = false;
    void asked!.then(() => { settled = true; });
    await Promise.resolve();
    await Promise.resolve();
    expect(settled, 'settled before the second watch heard the commit').toBe(false);
    await drain();
    expect(settled).toBe(true);
    expect(second.last()).toEqual(rows());
  });

  it("keeps the flush's other watches when a warning throws", async () => {
    watches = createWatches(connection, { clock: time.clock, warn: () => { throw new Error('the warning sink failed'); } });
    const costly = watching(20, 'costly');
    const after = watching(0.1, 'after');
    await drain();
    const errors: unknown[] = [];
    const onError = (error: unknown) => { errors.push(error); };
    process.on('uncaughtException', onError);
    try {
      for (let i = 0; i < 6; i++) {
        time.advance(1000);
        commit();
        await drain();
      }
    } finally {
      process.off('uncaughtException', onError);
    }
    expect(errors).toHaveLength(1);
    expect(costly.last()).toEqual(rows());
    expect(after.last(), 'the watch after the one that warned still heard every commit').toEqual(rows());
  });

  it('runs a watch once for a commit an earlier run in its flush made', async () => {
    // The first watch's read writes when told to; the second, still to run in that flush, reads the write
    let write = false;
    watches.watch(() => {
      if (write) {
        write = false;
        commit();
      }
      return rows();
    }, () => {}, { label: 'writes' });
    const reader = watching(0.1, 'reads');
    await drain();
    time.advance(1000);
    const before = reader.runs();
    write = true;
    commit();
    await drain();
    expect(reader.last()).toEqual(rows());
    expect(reader.runs() - before).toBe(1);
  });

  it('stops every watch quietly when its database is closed while one waits out its gap', async () => {
    const errors: unknown[] = [];
    let runs = 0;
    watches.watch(() => { runs++; time.spend(1); return rows(); }, { next: () => {}, error: (error) => { errors.push(error); } });
    await drain();
    commit();
    await turn();
    expect(time.pending(), 'a timer stands for the watch in its gap').toBe(1);
    const before = runs;
    connection.database.close();
    time.advance(WATCH_SCHEDULE.minimumGapMs * 4);
    await turn();
    expect(runs, 'nothing ran on the closed database').toBe(before);
    expect(errors).toEqual([]);
    expect(time.pending()).toBe(0);
    expect(await settledNow()).toBe(true);
    expect(watches.stats().watches).toBe(0);
  });
});

describe('waiting for one watch to catch up', () => {
  const gap = WATCH_SCHEDULE.minimumGapMs;

  it('resolves at once for a watch that has been handed everything', async () => {
    const watch = watching(1);
    await drain();
    expect(await resolved(watch.caughtUp())).toBe(true);
  });

  it("waits for a new watch's first run", async () => {
    const watch = watching(1);
    expect(watch.runs()).toBe(0);
    await watch.caughtUp();
    expect(watch.seen).toHaveLength(1);
  });

  it('resolves when the run that hands it the commit has ended, and not before', async () => {
    const watch = watching(1);
    await drain();
    commit();
    const waiting = watch.caughtUp();
    await turn();
    expect(await resolved(waiting), 'the watch is in its gap').toBe(false);
    time.advance(gap);
    await turn();
    expect(await resolved(waiting)).toBe(true);
    expect(watch.last()).toEqual(['Row-1']);
  });

  it('is not kept waiting by another watch that is behind', async () => {
    const cheap = watching(1);
    const costly = watching(40);
    await drain();
    commit();
    const waiting = cheap.caughtUp();
    const other = costly.caughtUp();
    await turn();
    expect(await resolved(waiting)).toBe(true);
    expect(cheap.last()).toEqual(['Row-1']);
    expect(await resolved(other), 'the costly watch is still in its gap').toBe(false);
    expect(await settledNow(), 'and so the watches are not settled').toBe(false);
  });

  it('is handed, by the one run it waits for, a second commit that landed before that run', async () => {
    const watch = watching(1);
    await drain();
    const before = watch.runs();
    commit();
    const waiting = watch.caughtUp();
    commit();
    time.advance(gap);
    await turn();
    expect(await resolved(waiting)).toBe(true);
    expect(watch.last()).toEqual(['Row-1', 'Row-2']);
    expect(watch.runs() - before).toBe(1);
  });

  it('stays resolved when a commit lands after its run, and a wait made then waits for the next', async () => {
    const watch = watching(1);
    await drain();
    commit();
    const first = watch.caughtUp();
    time.advance(gap);
    await turn();
    commit();
    expect(await resolved(first)).toBe(true);
    expect(await resolved(watch.caughtUp())).toBe(false);
  });

  it('answers every wait on one watch with the one run', async () => {
    const watch = watching(1);
    await drain();
    const before = watch.runs();
    commit();
    const waits = [watch.caughtUp(), watch.caughtUp(), watch.caughtUp()];
    time.advance(gap);
    await turn();
    expect(await Promise.all(waits.map(resolved))).toEqual([true, true, true]);
    expect(watch.runs() - before).toBe(1);
  });

  it('resolves when the run changed nothing, and delivered nothing', async () => {
    const seen: boolean[] = [];
    const any = watches.watch(() => { time.spend(1); return rows().length > 0; }, (value) => { seen.push(value); });
    commit();
    await drain();
    expect(seen).toEqual([true]);
    commit();
    const waiting = any.caughtUp();
    time.advance(gap);
    await turn();
    expect(await resolved(waiting)).toBe(true);
    expect(seen, 'there are still rows, so nothing was delivered').toEqual([true]);
  });

  it('resolves when the read threw', async () => {
    const errors: unknown[] = [];
    let fail = false;
    const watch = watches.watch(() => {
      time.spend(1);
      const found = rows();
      if (fail) throw new Error('the read broke');
      return found;
    }, { next: () => {}, error: (error) => { errors.push(error); } });
    await drain();
    fail = true;
    commit();
    const waiting = watch.caughtUp();
    time.advance(gap);
    await turn();
    expect(await resolved(waiting)).toBe(true);
    expect(errors).toHaveLength(1);
  });

  it('resolves when the watch is stopped, with nothing run', async () => {
    const watch = watching(1);
    await drain();
    const before = watch.runs();
    commit();
    const waiting = watch.caughtUp();
    watch.stop();
    expect(await resolved(waiting)).toBe(true);
    expect(watch.runs()).toBe(before);
    expect(await resolved(watch.caughtUp()), 'and a stopped watch is waited on for nothing').toBe(true);
  });

  it('resolves when the database is closed under it', async () => {
    const watch = watching(1);
    await drain();
    commit();
    const waiting = watch.caughtUp();
    connection.database.close();
    time.advance(gap * 4);
    await turn();
    expect(await resolved(waiting)).toBe(true);
  });

  it('goes on waiting for a watch a flush ran out of time for, until that watch has run', async () => {
    const first = watching(5, 'first');
    watching(5, 'second');
    const third = watching(5, 'third');
    const waiting = third.caughtUp();
    // The first flush runs one and has no time left for the others
    expect(await resolved(waiting)).toBe(false);
    expect([first.runs(), third.runs()]).toEqual([1, 0]);
    await drain();
    expect(await resolved(waiting)).toBe(true);
    expect(third.seen).toHaveLength(1);
  });

  it('keeps the timer for a gap as one to stay alive for while the watches are waited on to settle, and not after', async () => {
    watching(1);
    await drain();
    commit();
    await turn();
    expect(time.held(), 'nothing waits yet').toEqual([false]);
    const settling = watches.settled();
    expect(time.held(), 'the timer is set again, held').toEqual([true]);
    time.advance(gap);
    await turn();
    expect(await resolved(settling)).toBe(true);
    commit();
    await turn();
    expect(time.held(), 'and the next one is not').toEqual([false]);
  });

  it('keeps the timer for a gap as one to stay alive for while something waits on it, and not after', async () => {
    const watch = watching(1);
    await drain();
    commit();
    await turn();
    expect(time.held(), 'nothing waits yet').toEqual([false]);
    const waiting = watch.caughtUp();
    expect(time.held(), 'the timer is set again, held').toEqual([true]);
    time.advance(gap);
    await turn();
    expect(await resolved(waiting)).toBe(true);
    commit();
    await turn();
    expect(time.held(), 'and the next one is not').toEqual([false]);
  });

  it('said to be blocked on, is held to four times what the read costs and not to a frame', async () => {
    const factor = WATCH_SCHEDULE.delayFactor;
    const watch = watching(1);
    await drain();
    commit();
    const waiting = watch.caughtUp({ soon: true });
    time.advance(factor * 1 - 1);
    await turn();
    expect(await resolved(waiting), 'inside four times its cost').toBe(false);
    time.advance(1);
    await turn();
    expect(await resolved(waiting)).toBe(true);
    expect(watch.last()).toEqual(['Row-1']);
  });

  it('leaves a watch beside it, waited on without that word or not at all, to the frame', async () => {
    const blocked = watching(1, 'blocked on');
    const waited = watching(1, 'waited on');
    const alone = watching(1, 'not waited on');
    await drain();
    const before = alone.runs();
    commit();
    const soon = blocked.caughtUp({ soon: true });
    const plain = waited.caughtUp();
    // Past four times a read's cost for each of the three, and short of a frame for any
    time.advance(gap - 6);
    await turn();
    expect([await resolved(soon), await resolved(plain), alone.runs() - before]).toEqual([true, false, 0]);
    time.advance(gap);
    await turn();
    expect([await resolved(plain), alone.runs() - before]).toEqual([true, 1]);
  });

  it('lifts nothing for a wait on a watch that was owed nothing', async () => {
    const watch = watching(1);
    await drain();
    expect(await resolved(watch.caughtUp({ soon: true }))).toBe(true);
    const before = watch.runs();
    commit();
    time.advance(gap - 6);
    await turn();
    expect(watch.runs() - before, 'the commit after it waits out the frame, nothing being blocked on it').toBe(0);
    time.advance(gap);
    await turn();
    expect(watch.runs() - before).toBe(1);
  });

  it('holds a costly read to its own gap whether or not something is blocked on it', async () => {
    const cost = 10;
    const own = WATCH_SCHEDULE.delayFactor * cost;
    const watch = watching(cost);
    await drain();
    commit();
    const waiting = watch.caughtUp({ soon: true });
    time.advance(own - 1);
    await turn();
    expect(await resolved(waiting), `inside ${own} ms of its last run`).toBe(false);
    time.advance(1);
    await turn();
    expect(await resolved(waiting)).toBe(true);
  });

  it('gives the run after the one that was blocked on its whole gap again', async () => {
    const watch = watching(1);
    await drain();
    commit();
    const waiting = watch.caughtUp({ soon: true });
    time.advance(gap);
    await turn();
    expect(await resolved(waiting)).toBe(true);
    const before = watch.runs();
    commit();
    time.advance(gap - 2);
    await turn();
    expect(watch.runs() - before, 'nothing waits, so it is in its gap').toBe(0);
    time.advance(2);
    await turn();
    expect(watch.runs() - before).toBe(1);
  });

  it('brings forward a flush put off to a later turn, for a watch something is blocked on and for no other wait', async () => {
    watching(5, 'first');
    watching(5, 'second');
    const third = watching(5, 'third');
    // The first flush has no time for the third, and puts the rest off to a later turn of the event loop
    expect(await resolved(third.caughtUp())).toBe(false);
    expect(await resolved(third.caughtUp()), 'a wait nothing is blocked on leaves the flush where it was put').toBe(false);
    expect(third.runs()).toBe(0);
    expect(await resolved(third.caughtUp({ soon: true })), 'run without the event loop turning').toBe(true);
    expect(third.runs()).toBe(1);
    await drain();
    expect(third.runs(), 'the flush that was put off runs nothing twice').toBe(1);
  });

  it('runs what is blocked on first in a flush that has no time for all of it', async () => {
    const ran: string[] = [];
    const reading = (label: string) => watches.watch(() => { ran.push(label); time.spend(5); return rows(); }, () => {});
    reading('first');
    reading('second');
    const third = reading('third');
    const waiting = third.caughtUp({ soon: true });
    expect(await resolved(waiting)).toBe(true);
    expect(ran[0], 'the one blocked on ran before those made before it').toBe('third');
    await drain();
    expect(ran).toEqual(['third', 'first', 'second']);
  });

  it('holds for any run of commits, waits, stops and time passing: every wait ends, having been handed what was committed before it', async () => {
    type Op = { do: 'commit' } | { do: 'wait'; watch: number; soon: boolean } | { do: 'pass'; ms: number } | { do: 'turn' } | { do: 'stop'; watch: number };
    const costs = [1, 5, 20];
    const op = fc.oneof(
      { weight: 4, arbitrary: fc.constant<Op>({ do: 'commit' }) },
      { weight: 4, arbitrary: fc.tuple(fc.nat(costs.length - 1), fc.boolean()).map(([watch, soon]): Op => ({ do: 'wait', watch, soon })) },
      { weight: 3, arbitrary: fc.nat(40).map((ms): Op => ({ do: 'pass', ms })) },
      { weight: 3, arbitrary: fc.constant<Op>({ do: 'turn' }) },
      { weight: 1, arbitrary: fc.nat(costs.length - 1).map((watch): Op => ({ do: 'stop', watch })) },
    );
    await fc.assert(fc.asyncProperty(fc.array(op, { minLength: 5, maxLength: 60 }), async (ops) => {
      fresh();
      const stopped = costs.map(() => false);
      /** Whether something has said it is blocked on each watch since that watch last ran */
      const blockedOn = costs.map(() => false);
      /** Whether each watch is owed a run, as the spec itself knows it: never run, or committed to since it last began one */
      const owedARun = costs.map(() => true);
      /** When each watch's last run ended and what it cost, to hold the next run to its gap */
      const gaps: string[] = [];
      const made = costs.map((cost, at) => {
        const seen: string[][] = [];
        let endedAt = -1;
        const handle = watches.watch(() => {
          const began = time.clock.now();
          const held = Math.max(blockedOn[at] ? 0 : gap, WATCH_SCHEDULE.delayFactor * cost);
          if (endedAt >= 0 && began < endedAt + held) gaps.push(`watch ${at} ran ${began - endedAt} ms after a run that cost ${cost}, ${blockedOn[at] ? 'blocked on' : 'not blocked on'}`);
          blockedOn[at] = false;
          owedARun[at] = false;
          time.spend(cost);
          endedAt = time.clock.now();
          return rows();
        }, (value) => { seen.push(value); });
        return { handle, seen };
      });
      const waits: Array<{ watch: number; committed: number; ended?: { handed: number; stopped: boolean } }> = [];
      for (const each of ops) {
        if (each.do === 'commit') { commit(); owedARun.fill(true); }
        else if (each.do === 'pass') time.advance(each.ms);
        else if (each.do === 'turn') await turn();
        else if (each.do === 'stop') { stopped[each.watch] = true; made[each.watch]!.handle.stop(); }
        else {
          const wait: (typeof waits)[number] = { watch: each.watch, committed: sequence };
          waits.push(wait);
          // A wait on a watch that is owed nothing, or is stopped, resolves at once and lifts nothing
          if (each.soon && owedARun[each.watch] && !stopped[each.watch]) blockedOn[each.watch] = true;
          void made[each.watch]!.handle.caughtUp({ soon: each.soon }).then(() => {
            wait.ended = { handed: made[wait.watch]!.seen.at(-1)?.length ?? 0, stopped: stopped[wait.watch]! };
          });
        }
      }
      // Time passes and nothing more is committed: every wait has to end
      for (let i = 0; i < 200 && waits.some((wait) => !wait.ended); i++) {
        await turn();
        time.advance(gap);
      }
      expect(waits.filter((wait) => !wait.ended), 'waits that never ended').toEqual([]);
      expect(waits.filter((wait) => !wait.ended!.stopped && wait.ended!.handed < wait.committed), 'waits that ended before their commits were handed over').toEqual([]);
      expect(gaps, 'runs inside a gap').toEqual([]);
      for (const { handle } of made) handle.stop();
    }), { numRuns: 80 });
  });

  it('tracks the earliest dirty deadline across stops, soon lifts, and commit bursts', async () => {
    const fast = watching(1, 'fast');
    const medium = watching(5, 'medium');
    const slow = watching(10, 'slow');
    await drain();
    time.advance(1000);

    // One commit puts all three into overlapping gaps: fast + medium fit in the first 8 ms slice, slow runs next turn
    // fast ranAt +1 (due +17), medium ranAt +6 (due +26), slow ranAt +16 (due +56)
    commit();
    await turn();
    await turn();
    expect([fast.last()?.length, medium.last()?.length, slow.last()?.length]).toEqual([1, 1, 1]);

    // Commit burst marks all three dirty while all three are in their gaps
    for (let i = 0; i < 20; i++) commit();
    await turn();

    // Stopping the earliest watch (fast) must invalidate and recompute so medium runs at +26 and not before
    fast.stop();
    time.advance(5);
    await turn();
    expect(medium.last(), 'medium is still inside its 20 ms gap').toHaveLength(1);
    time.advance(5);
    await turn();
    expect(medium.last()).toHaveLength(21);
    expect(slow.last(), 'slow is still inside its 40 ms gap').toHaveLength(1);
    time.advance(30);
    await turn();
    expect(slow.last()).toHaveLength(21);

    // Lifting `soon` on a watch with a shorter cost-based gap must pull the earliest deadline ahead of slower dirty watches
    const cheap = watching(0.5, 'cheap');
    await drain();
    time.advance(1000);
    commit();
    await turn();
    await turn();
    commit();
    await turn();
    const waitSoon = cheap.caughtUp({ soon: true });
    time.advance(2);
    await turn();
    expect(await resolved(waitSoon)).toBe(true);
    expect(cheap.last()).toHaveLength(23);
    await drain();
    medium.stop();
    slow.stop();
    cheap.stop();
  });
});
