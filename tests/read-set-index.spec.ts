// The read-set index against a brute-force scan: random read sets filed, replaced and removed, then random written keys,
// and the owners the index finds for each are exactly those a scan of every read set finds. Keys are short strings over
// a few byte values, so ranges meet, nest and share ends often; a range's end is exclusive, and an absent end runs to
// the end of the store.
import fc from 'fast-check';
import { describe, expect, it } from 'vitest';
import { createReadSetIndex } from '../src/read-set-index.ts';
import type { ReadSet } from '../src/kv.ts';

const key = fc.string({ unit: fc.constantFrom('\u0000', 'a', 'b', 'ÿ'), maxLength: 3 });
const readSet: fc.Arbitrary<ReadSet> = fc.record({
  keys: fc.uniqueArray(key, { maxLength: 4 }).map((keys) => new Set(keys)),
  ranges: fc.array(fc.tuple(key, fc.option(key, { nil: undefined })), { maxLength: 4 }),
});

/** Who a brute-force scan finds for a written key */
const scan = (sets: Map<number, ReadSet>, written: string) => new Set([...sets].filter(([, reads]) =>
  reads.keys.has(written) || reads.ranges.some(([low, high]) => low <= written && (high === undefined || written < high))).map(([owner]) => owner));

type Step = { set: [number, ReadSet] } | { remove: number } | { write: string[] };
const step: fc.Arbitrary<Step> = fc.oneof(
  fc.record({ set: fc.tuple(fc.integer({ min: 0, max: 7 }), readSet) }),
  fc.record({ remove: fc.integer({ min: 0, max: 7 }) }),
  fc.record({ write: fc.array(key, { minLength: 1, maxLength: 3 }) }),
);

describe('the read-set index', () => {
  it('finds exactly the owners a scan of every read set finds, through sets, replacements and removals', () => {
    fc.assert(fc.property(fc.array(step, { maxLength: 40 }), (steps) => {
      const index = createReadSetIndex<number>();
      const sets = new Map<number, ReadSet>();
      for (const s of steps) {
        if ('set' in s) { index.set(s.set[0], s.set[1]); sets.set(s.set[0], s.set[1]); }
        else if ('remove' in s) { index.remove(s.remove); sets.delete(s.remove); }
        else {
          const expected = new Set<number>();
          for (const written of s.write) for (const owner of scan(sets, written)) expected.add(owner);
          expect(index.touchedBy(new Set(s.write))).toEqual(expected);
        }
      }
      expect(index.touchedBy('all')).toEqual(new Set(sets.keys()));
      for (const owner of sets.keys()) index.remove(owner);
      expect(index.size()).toEqual({ keys: 0, ranges: 0 });
    }), { numRuns: 500 });
  });

  it('finds a range by a key at its low end and not at its high end', () => {
    const index = createReadSetIndex<string>();
    index.set('r', { keys: new Set(), ranges: [['b', 'd']] });
    index.set('open', { keys: new Set(), ranges: [['c', undefined]] });
    expect(index.touchedBy(new Set(['b']))).toEqual(new Set(['r']));
    expect(index.touchedBy(new Set(['d']))).toEqual(new Set(['open']));
    expect(index.touchedBy(new Set(['ÿÿ']))).toEqual(new Set(['open']));
    expect(index.touchedBy(new Set(['a']))).toEqual(new Set());
  });
});
