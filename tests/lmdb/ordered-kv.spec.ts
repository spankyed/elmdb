// The ordered key-value seam's contract, run against both backends: what holds on memory holds on LMDB, which is what
// makes a test on memory a test of the app
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { createMemoryKV, type Key, type OrderedKV, type RangeOptions } from '../../src/kv.ts';
import { openLmdbStore, type LmdbStore } from '../../src/lmdb/index.ts';

const opened: Array<{ store: LmdbStore; dir: string }> = [];
afterEach(() => {
  for (const { store, dir } of opened.splice(0)) {
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

/** An LMDB store in a directory of its own */
function lmdbStore(): LmdbStore {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ordered-kv-'));
  const store = openLmdbStore({ path: path.join(dir, 'ears-db') });
  opened.push({ store, dir });
  return store;
}
const lmdbKV = (): OrderedKV => lmdbStore().kv;

/** The keys a range visits, in one snapshot */
const keysOf = (kv: OrderedKV, options: RangeOptions) =>
  kv.read((r) => {
    const keys: Key[] = [];
    r.range(options, (key) => { keys.push(key); });
    return keys;
  });

describe.each([
  ['memory', createMemoryKV],
  ['LMDB', lmdbKV],
] as const)('an ordered KV on %s', (_name, make) => {
  it('orders array keys as LMDB does: null, booleans, numbers, strings, element by element', () => {
    const kv = make();
    const keys: Key[] = [['b'], ['a', 'x'], ['a', 10], ['a', 2], ['ab'], ['a', true], ['a', null], ['a', 1], ['a', 'x', 1]];
    kv.write((w) => { for (const key of keys) w.put(key, key.join('.')); });
    expect(keysOf(kv, {})).toEqual([['a', null], ['a', true], ['a', 1], ['a', 2], ['a', 10], ['a', 'x'], ['a', 'x', 1], ['ab'], ['b']]);
  });

  it('orders numbers by value, negatives and fractions included, and strings by their UTF-8; refuses NaN and NUL', () => {
    const kv = make();
    const numbers = [3, -0.5, 1e9, -5, 0, 0.5, -1e9, 2 ** 53];
    const strings = ['é', 'z', 'Z', 'a', 'ab', '日本', ''];
    kv.write((w) => {
      for (const n of numbers) w.put(['n', n], n);
      for (const t of strings) w.put(['s', t], t);
      w.put(['n', -0], 'negative zero');
    });
    expect(keysOf(kv, { prefix: ['n'] }).map((key) => key[1])).toEqual([-1e9, -5, -0.5, 0, 0.5, 3, 1e9, 2 ** 53]);
    expect(kv.read((r) => r.get(['n', 0]))).toBe('negative zero');
    expect(keysOf(kv, { prefix: ['s'] }).map((key) => key[1])).toEqual(['', 'Z', 'a', 'ab', 'z', 'é', '日本']);
    expect(() => kv.write((w) => w.put(['n', Number.NaN], 1))).toThrow('NaN');
    expect(() => kv.write((w) => w.put(['s', 'a\u0000b'], 1))).toThrow('NUL');
  });

  it('ranges by prefix, bounds, direction and limit, and reads keys alone', () => {
    const kv = make();
    kv.write((w) => {
      for (let i = 1; i <= 5; i++) w.put(['e', 'Task', i], `Task-${i}`);
      w.put(['e', 'Tasks', 1], 'other');
      w.put(['e', 'Task'], 'the prefix itself');
    });
    expect(keysOf(kv, { prefix: ['e', 'Task'] })).toEqual([1, 2, 3, 4, 5].map((i) => ['e', 'Task', i]));
    expect(keysOf(kv, { prefix: ['e', 'Task'], reverse: true, limit: 2 })).toEqual([['e', 'Task', 5], ['e', 'Task', 4]]);
    expect(keysOf(kv, { prefix: ['e', 'Task'], start: [2], end: [4] })).toEqual([['e', 'Task', 2], ['e', 'Task', 3]]);
    expect(keysOf(kv, { prefix: ['e', 'Task'], start: [2], end: [4], reverse: true })).toEqual([['e', 'Task', 3], ['e', 'Task', 2]]);
    expect(keysOf(kv, { start: ['e', 'Task', 5], end: ['e', 'Tasks'] })).toEqual([['e', 'Task', 5]]);
    const visited: unknown[] = [];
    kv.read((r) => r.range({ prefix: ['e', 'Task'], values: false, limit: 1 }, (key, value) => { visited.push(key, value); }));
    expect(visited).toEqual([['e', 'Task', 1], undefined]);
  });

  it('gives back a copy of what was stored: dates, nested values and undefined fields included', () => {
    const kv = make();
    const value = { at: new Date('2026-01-01T00:00:00Z'), list: [1, 'two', { three: 3 }], gone: undefined, nil: null };
    kv.write((w) => w.put(['v', 'a'], value));
    const read = kv.read((r) => r.get(['v', 'a'])) as typeof value;
    expect(read).toEqual(value);
    expect(read.at).toBeInstanceOf(Date);
    (read.list as unknown[]).push('changed');
    expect(kv.read((r) => r.get(['v', 'a']))).toEqual(value);
    expect(kv.read((r) => r.get(['v', 'missing']))).toBeUndefined();
  });

  it('gives back a field named __proto__ as a field, beside one named __proto_', () => {
    const kv = make();
    const value = JSON.parse('{"__proto__": {"polluted": "yes"}, "__proto_": 1, "nested": [{"__proto__": 2}]}');
    kv.write((w) => w.put(['v', 'p'], value));
    const read = kv.read((r) => r.get(['v', 'p'])) as Record<string, unknown>;
    expect(Object.getPrototypeOf(read)).toBe(Object.prototype);
    expect(Object.getOwnPropertyDescriptor(read, '__proto__')?.value).toEqual({ polluted: 'yes' });
    expect(read.__proto_).toBe(1);
    expect(Object.getOwnPropertyDescriptor((read.nested as object[])[0], '__proto__')?.value).toBe(2);
    expect(({} as Record<string, unknown>).polluted).toBeUndefined();
  });

  it("isolates a snapshot: a commit made while it is open isn't in it", () => {
    const kv = make();
    kv.write((w) => w.put(['k'], 1));
    const seen = kv.read((r) => {
      kv.write((w) => { w.put(['k'], 2); w.put(['new'], true); });
      const keys: Key[] = [];
      r.range({}, (key) => { keys.push(key); });
      return { value: r.get(['k']), keys };
    });
    expect(seen).toEqual({ value: 1, keys: [['k']] });
    expect(kv.read((r) => [r.get(['k']), r.get(['new'])])).toEqual([2, true]);
  });

  it('reads its own writes inside a unit, ranges included, and a read inside it is the unit', () => {
    const kv = make();
    kv.write((w) => { w.put(['e', 1], 'a'); w.put(['e', 3], 'c'); });
    kv.write((w) => {
      w.put(['e', 2], 'b');
      w.del(['e', 3]);
      w.put(['e', 1], 'A');
      expect(w.get(['e', 1])).toBe('A');
      const entries: unknown[] = [];
      w.range({ prefix: ['e'] }, (key, value) => { entries.push([key, value]); });
      expect(entries).toEqual([[['e', 1], 'A'], [['e', 2], 'b']]);
      expect(kv.read((r) => r.get(['e', 2]))).toBe('b');
      const reversed: Key[] = [];
      w.range({ prefix: ['e'], reverse: true }, (key) => { reversed.push(key); });
      expect(reversed).toEqual([['e', 2], ['e', 1]]);
    });
    expect(keysOf(kv, { prefix: ['e'] })).toEqual([['e', 1], ['e', 2]]);
  });

  it('keeps nothing of a unit that throws, and of a nested one only what it wrote before the throw is undone', () => {
    const kv = make();
    kv.write((w) => w.put(['k'], 'before'));
    expect(() => kv.write((w) => { w.put(['k'], 'during'); w.put(['other'], 1); throw new Error('stop'); })).toThrow('stop');
    expect(kv.read((r) => [r.get(['k']), r.get(['other'])])).toEqual(['before', undefined]);
    kv.write((w) => {
      w.put(['outer'], 1);
      expect(() => kv.write((inner) => { inner.put(['inner'], 1); inner.put(['outer'], 2); inner.del(['k']); throw new Error('inner'); })).toThrow('inner');
      expect([w.get(['outer']), w.get(['inner']), w.get(['k'])]).toEqual([1, undefined, 'before']);
      kv.write((inner) => inner.put(['kept'], true));
    });
    expect(kv.read((r) => [r.get(['outer']), r.get(['inner']), r.get(['kept']), r.get(['k'])])).toEqual([1, undefined, true, 'before']);
    // An outer unit that throws undoes a nested one that committed into it
    expect(() => kv.write(() => { kv.write((inner) => inner.put(['nested'], 1)); throw new Error('outer'); })).toThrow('outer');
    expect(kv.read((r) => r.get(['nested']))).toBeUndefined();
  });

  it('keeps thousands of keys written out of order sorted, through removals, ranged both ways', () => {
    const kv = make();
    // A fixed shuffle: each number in 0..4999 once, in no order
    const numbers = Array.from({ length: 5_000 }, (_, i) => (i * 7_919) % 5_000);
    kv.write((w) => { for (const n of numbers) w.put(['k', `n${String(n).padStart(4, '0')}`], n); });
    kv.write((w) => { for (const n of numbers) if (n % 3 === 0) w.del(['k', `n${String(n).padStart(4, '0')}`]); });
    const kept = Array.from({ length: 5_000 }, (_, n) => n).filter((n) => n % 3 !== 0);
    const values = (options: RangeOptions) => kv.read((r) => {
      const seen: unknown[] = [];
      r.range(options, (_key, value) => { seen.push(value); });
      return seen;
    });
    expect(values({ prefix: ['k'] })).toEqual(kept);
    expect(values({ prefix: ['k'], reverse: true })).toEqual([...kept].reverse());
    const within = kept.filter((n) => n >= 1_234 && n < 3_456);
    expect(values({ prefix: ['k'], start: ['n1234'], end: ['n3456'] })).toEqual(within);
    expect(values({ prefix: ['k'], start: ['n1234'], end: ['n3456'], reverse: true })).toEqual([...within].reverse());
    // A unit's own writes merged with the committed keys, across chunks
    kv.write((w) => {
      w.put(['k', 'n0000'], 0);
      w.del(['k', 'n0001']);
      const seen: unknown[] = [];
      w.range({ prefix: ['k'], limit: 3 }, (_key, value) => { seen.push(value); });
      expect(seen).toEqual([0, 2, 4]);
    });
  });

  it('closes a range however its visit ends, so a thousand that stop early or throw leave it working', () => {
    const kv = make();
    kv.write((w) => { for (let i = 0; i < 10; i++) w.put(['e', i], i); });
    for (let i = 0; i < 1_000; i++) {
      kv.read((r) => r.range({ prefix: ['e'] }, () => false));
      expect(() => kv.read((r) => r.range({ prefix: ['e'] }, () => { throw new Error('visit'); }))).toThrow('visit');
    }
    kv.write((w) => w.put(['after'], true));
    expect(kv.read((r) => r.get(['after']))).toBe(true);
  });
});

describe('an ordered KV on LMDB', () => {
  it('releases every snapshot: reads and writes interleaved leave one reader at most, the shared one', () => {
    const store = lmdbStore();
    const { kv } = store;
    for (let i = 0; i < 50; i++) {
      kv.write((w) => w.put(['n', i], i));
      kv.read((r) => r.range({ prefix: ['n'] }, () => false));
      expect(() => kv.read((r) => r.range({ prefix: ['n'] }, () => { throw new Error('visit'); }))).toThrow('visit');
    }
    expect(store.readers()).toBeLessThanOrEqual(1);
  });
});
