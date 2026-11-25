import { expect, it } from 'vitest';
import type { Commit, CommitSource, Connection } from '../src/database.ts';
import { createWatches, type WatchReadContext } from '../src/watch.ts';

// A scheduler-only connection: no native code or database is opened by these cause-metadata checks.
function fixture(recordCauses?: () => boolean) {
  let sequence = 0;
  let listener: ((commit: Commit) => void) | undefined;
  const connection = {
    database: { isOpen: true }, untrack() {}, tally: () => undefined, callSite: () => 'a scheduler spec',
    track: (_token: number, read: () => unknown) => read(),
    lastCommit: () => sequence, trackStats: () => ({ keys: 0, ranges: 0 }),
    onCommit(next: (commit: Commit) => void) { listener = next; return () => { listener = undefined; }; },
  } as unknown as Connection;
  const watches = createWatches(connection, { schedule: { minimumGapMs: 0, delayFactor: 0, flushSliceMs: 100 } });
  const contexts: Array<WatchReadContext | undefined> = [];
  const values: number[] = [];
  const watch = watches.watch(context => { contexts.push(context); return sequence; }, value => values.push(value), { recordCauses });
  return { watches, contexts, values, stop: watch.stop,
    commit(touched: ReadonlySet<number> | 'all' = new Set([1]), source: CommitSource = 'local') {
      listener?.({ sequence: ++sequence, touched, source });
    },
  };
}
it('passes only the exact affected commits to one coalesced read, with no initial invented cause', async () => {
  const f = fixture(() => true);
  await f.watches.settled();
  f.commit(); f.commit(new Set([999])); f.commit();
  await f.watches.settled();
  expect(f.contexts).toEqual([undefined, { sequences: [1, 3], dropped: 0, sources: ['local'], wildcard: false }]);
  expect(Object.isFrozen(f.contexts[1])).toBe(true);
  expect(Object.isFrozen(f.contexts[1]!.sequences)).toBe(true);
  expect(Object.isFrozen(f.contexts[1]!.sources)).toBe(true);
  expect(f.values).toEqual([0, 3]);
  f.stop();
});
it('records distinct payload-free sources and wildcard invalidation flags across coalesced commits', async () => {
  const f = fixture(() => true);
  await f.watches.settled();
  f.commit(new Set([1]), 'local');
  f.commit(new Set([1]), 'builder');
  f.commit('all', 'elsewhere');
  f.commit(new Set([1]), 'builder');
  await f.watches.settled();
  expect(f.contexts[1]).toEqual({
    sequences: [1, 2, 3, 4],
    dropped: 0,
    sources: ['local', 'builder', 'elsewhere'],
    wildcard: true,
  });
  expect(Object.isFrozen(f.contexts[1]!.sources)).toBe(true);
  f.stop();
});
it('bounds retained causes and reports losses without changing the read result', async () => {
  const f = fixture(() => true);
  await f.watches.settled();
  for (let index = 0; index < 40; index++) f.commit();
  await f.watches.settled();
  expect(f.contexts[1]!.sequences).toHaveLength(32);
  expect(f.contexts[1]!.dropped).toBe(8);
  expect(f.contexts[1]!.sources).toEqual(['local']);
  expect(f.contexts[1]!.wildcard).toBe(false);
  expect(f.values).toEqual([0, 40]);
  f.stop();
});
it('keeps recording opt-in and preserves reads if the diagnostic policy fails', async () => {
  const disabled = fixture();
  await disabled.watches.settled(); disabled.commit(); await disabled.watches.settled();
  expect(disabled.contexts).toEqual([undefined, undefined]); disabled.stop();
  const failed = fixture(() => { throw Error('diagnostic policy'); });
  await failed.watches.settled(); failed.commit(); await failed.watches.settled();
  expect(failed.values).toEqual([0, 1]);
  expect(failed.contexts[1]).toEqual({ sequences: [], dropped: 1, sources: [], wildcard: false }); failed.stop();
});
