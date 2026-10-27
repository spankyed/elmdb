// The watch scheduler's guarantees under any write rate (G14 of elmdb's goal-elmdb-next.md), on a clock the spec
// moves: a read's cost is what it advances the clock by, so a run is as cheap or as expensive as a case says, on any
// machine. Commits are real ones, to a real database; turns of the event loop are real too.
import { afterAll, beforeEach, describe, expect, it } from 'vitest';
import { createConnection, type Connection } from '../../src/database.ts';
import { createWatches, WATCH_SCHEDULE, type WatchClock, type Watches } from '../../src/watch.ts';
import { createTemporaryDatabases } from '../../src/lmdb/index.ts';

const databases = createTemporaryDatabases();
afterAll(() => databases.disposeAll());

/** A clock that moves only when told, firing the timers it passes */
function fakeClock() {
  let now = 0;
  let next = 0;
  const timers = new Map<number, { at: number; fn: () => void }>();
  const clock: WatchClock = {
    now: () => now,
    setTimeout: (fn, ms) => {
      timers.set(++next, { at: now + ms, fn });
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
  };
}

/** A turn of the event loop: microtasks, then a later macrotask's work */
const turn = () => new Promise<void>((resolve) => setImmediate(resolve));

let connection: Connection;
let time: ReturnType<typeof fakeClock>;
let watches: Watches;
let warnings: string[];
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
  return { seen, runs: () => runs, last: () => seen.at(-1), stop: () => handle.stop() };
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

beforeEach(() => {
  const { database } = databases.open({ types: { Row: { required: [] } }, relations: {}, roles: [] });
  connection = createConnection(database);
  time = fakeClock();
  warnings = [];
  watches = createWatches(connection, { clock: time.clock, warn: (message) => warnings.push(message) });
  sequence = 0;
});

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
});
