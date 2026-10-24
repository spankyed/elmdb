// When `durable()` resolves: once a sync that began after the last commit has finished, and never on a sync that
// failed. The environment's sync is the spec's to finish or fail (lmdb's `open` is wrapped, so each store's `sync`
// queues its callback here), since what is under test is which sync each promise waits for, not the flush itself;
// the store's timers are faked so no sync starts unasked.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createEarsEngine, type EarsEngine } from '../../src/index.ts';
import { openLmdbStore, SYNC_IDLE_MS, type LmdbStore } from '../../src/lmdb/index.ts';

/** The syncs the stores started and the spec has not settled, oldest first */
const pending = vi.hoisted(() => [] as Array<(error?: Error) => void>);

vi.mock('lmdb', async (importOriginal) => {
  const lmdb = await importOriginal<typeof import('lmdb')>();
  return {
    ...lmdb,
    open: (...args: Parameters<typeof lmdb.open>) => {
      const root = lmdb.open(...args);
      (root as unknown as { sync(done: (error?: Error) => void): void }).sync = (done) => { pending.push(done); };
      return root;
    },
  };
});

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-durability-'));
afterAll(() => fs.rmSync(root, { recursive: true, force: true }));

let store: LmdbStore;
let engine: EarsEngine;
beforeEach(() => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
  pending.length = 0;
  store = openLmdbStore({ path: path.join(fs.mkdtempSync(path.join(root, 'case-')), 'ears-db') });
  engine = createEarsEngine({ kv: store.kv, isEntityType: (name) => name === 'Note', isRelationKind: () => true, isRole: () => true });
});
afterEach(() => {
  // Close commits its own sync, which the spec's queue would otherwise hold
  for (const done of pending.splice(0)) done();
  store.close();
  vi.useRealTimers();
});

/** Settles the oldest sync the store started: it finished, or it failed with `error` */
const finishSync = (error?: Error) => {
  const done = pending.shift();
  if (!done) throw new Error('no sync is running');
  done(error);
};

/** Whether a promise has settled, and how, once the microtasks queued before asking have run */
async function outcome(promise: Promise<unknown>): Promise<'pending' | 'resolved' | 'rejected'> {
  let state: 'pending' | 'resolved' | 'rejected' = 'pending';
  promise.then(() => { state = 'resolved'; }, () => { state = 'rejected'; });
  for (let i = 0; i < 5; i++) await Promise.resolve();
  return state;
}

describe('durable()', () => {
  it('waits for a sync begun after the write, and a write made while that sync runs waits for the next', async () => {
    engine.create('Note', { title: 'first' });
    const first = store.durable();
    expect(pending, 'a sync starts at once, not at the timer').toHaveLength(1);

    engine.create('Note', { title: 'while the first sync runs' });
    const second = store.durable();
    expect(pending, 'the running sync began before the second write, so it cannot cover it').toHaveLength(1);

    finishSync();
    expect(await outcome(first)).toBe('resolved');
    expect(await outcome(second), 'the first sync finishing is not the second write on disk').toBe('pending');
    expect(pending, 'the next sync starts once the first ends').toHaveLength(1);

    finishSync();
    expect(await outcome(second)).toBe('resolved');
  });

  it('shares the running sync with a caller that committed nothing since it began', async () => {
    engine.create('Note', { title: 'one write' });
    const asked = store.durable();
    const joined = store.durable();
    expect(pending).toHaveLength(1);
    finishSync();
    expect(await outcome(asked)).toBe('resolved');
    expect(await outcome(joined)).toBe('resolved');
  });

  it('rejects when its sync fails, for every caller waiting on it, and the next call syncs again', async () => {
    engine.create('Note', { title: 'not yet on disk' });
    const asked = store.durable();
    const joined = store.durable();
    finishSync(new Error('EIO'));
    expect(await outcome(asked)).toBe('rejected');
    expect(await outcome(joined), 'a caller sharing the failed sync is told so too').toBe('rejected');

    // The write is still not known to be on disk, so the next call is a sync, not an answer from memory
    const retry = store.durable();
    expect(pending, 'a failed sync leaves its commits owed').toHaveLength(1);
    expect(await outcome(retry)).toBe('pending');
    finishSync();
    expect(await outcome(retry)).toBe('resolved');
  });

  it('runs a sync queued behind one that failed, and the caller waiting on it hears only its own', async () => {
    engine.create('Note', { title: 'first' });
    const first = store.durable();
    engine.create('Note', { title: 'while the first sync runs' });
    const second = store.durable();
    finishSync(new Error('EIO'));
    expect(await outcome(first)).toBe('rejected');
    expect(await outcome(second), "the first sync's failure is not the second caller's").toBe('pending');
    expect(pending, 'the queued sync starts, and takes what the failed one left owed').toHaveLength(1);
    finishSync();
    expect(await outcome(second)).toBe('resolved');
  });

  it('keeps a failed background sync from reaching the process, and its commits owed', async () => {
    engine.create('Note', { title: 'first' });
    const asked = store.durable();
    engine.create('Note', { title: 'while that sync runs' });
    // Group commit's timer asks while a sync runs, so its sync queues behind it, and nobody waits on that one
    vi.advanceTimersByTime(SYNC_IDLE_MS);
    finishSync();
    await asked;
    for (let i = 0; i < 5; i++) await Promise.resolve();
    expect(pending, 'the queued sync started').toHaveLength(1);
    // A failure that escaped here would be an unhandled rejection, which vitest fails the run on
    finishSync(new Error('EIO'));
    for (let i = 0; i < 5; i++) await Promise.resolve();

    const retry = store.durable();
    expect(pending, 'the failed sync made nothing durable, so asking starts another').toHaveLength(1);
    finishSync();
    expect(await outcome(retry)).toBe('resolved');
  });

  it('starts no sync when nothing was committed since the last one finished', async () => {
    engine.create('Note', { title: 'synced' });
    const asked = store.durable();
    finishSync();
    await asked;
    expect(await outcome(store.durable())).toBe('resolved');
    expect(pending).toHaveLength(0);
  });
});
