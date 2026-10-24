// The ordered key-value seam's contract, run against both backends: what holds on memory holds on LMDB, which is what
// makes a test on memory a test of the app
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { open, type RootDatabase } from 'lmdb';
import { afterEach, describe, expect, it } from 'vitest';
import { createMemoryKV, createOrderedKV, type Key, type OrderedKV, type RangeOptions } from '../../src/kv.ts';
import { createLmdbKVBackend } from '../../src/lmdb/kv-backend.ts';

const opened: Array<{ root: RootDatabase; dir: string }> = [];
afterEach(() => {
  for (const { root, dir } of opened.splice(0)) {
    root.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

function lmdbKV(): OrderedKV {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ordered-kv-'));
  const root = open({ path: dir, overlappingSync: false });
  opened.push({ root, dir });
  return createOrderedKV(createLmdbKVBackend(root, root.openDB({ name: 'kv', keyEncoding: 'binary', encoding: 'binary' })));
}

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
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ordered-kv-'));
    const root = open({ path: dir, overlappingSync: false });
    opened.push({ root, dir });
    const kv = createOrderedKV(createLmdbKVBackend(root, root.openDB({ name: 'kv', keyEncoding: 'binary', encoding: 'binary' })));
    for (let i = 0; i < 50; i++) {
      kv.write((w) => w.put(['n', i], i));
      kv.read((r) => r.range({ prefix: ['n'] }, () => false));
      expect(() => kv.read((r) => r.range({ prefix: ['n'] }, () => { throw new Error('visit'); }))).toThrow('visit');
    }
    // One line per reader slot this process holds, under a header
    const readers = root.readerList().trim().split('\n').slice(1);
    expect(readers.length).toBeLessThanOrEqual(1);
  });
});
