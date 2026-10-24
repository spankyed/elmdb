// Record stores: whole records by id, looked up by indexed fields in creation order. One on disk (openRecordStore)
// and one in memory (createMemoryRecordStore) are the same RecordStore, so what both do runs against both
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterAll, afterEach, beforeEach, describe, expect, it } from 'vitest';
import { openEnvironment, openRecordStore } from '../../src/lmdb/index.ts';
import { createMemoryRecordStore, type RecordIndexes, type RecordStore, type StoredRecord } from '../../src/index.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-record-store-'));
let dir: string;
const open: RecordStore[] = [];
const INDEXES: RecordIndexes = {
  parentId: (record) => record.parentId,
  status: (record) => record.status,
  // Computed: filed only when it derives a value, so nothing is stored just to be found by
  isRoot: (record) => record.parentId === undefined || undefined,
};
const store = (options: { readOnly?: boolean; at?: string } = {}) => {
  const opened = openRecordStore({ path: options.at ?? dir, indexes: INDEXES, readOnly: options.readOnly });
  open.push(opened);
  return opened;
};
const STORES: Record<string, () => RecordStore> = {
  'on disk': () => store(),
  'in memory': () => createMemoryRecordStore({ indexes: INDEXES }),
};
const ids = (records: StoredRecord[]) => records.map((record) => record.id);

beforeEach(() => { dir = path.join(fs.mkdtempSync(path.join(root, 'case-')), 'records'); });
afterEach(() => { for (const opened of open.splice(0)) opened.close(); });
afterAll(() => fs.rmSync(root, { recursive: true, force: true }));

describe.each(Object.keys(STORES))('a record store %s', (where) => {
  const fresh = STORES[where];

  it('reads a record back as written, and forgets it when deleted', () => {
    const s = fresh();
    s.put({ id: 'R-1', status: 'active', payload: { big: [1, 2] } });
    expect(s.get('R-1')).toEqual({ id: 'R-1', status: 'active', payload: { big: [1, 2] } });
    s.delete('R-1');
    s.delete('R-unknown');
    expect(s.get('R-1')).toBeUndefined();
    expect(s.findBy('status', 'active')).toEqual([]);
  });

  it('keeps each index to the value a record holds now', () => {
    const s = fresh();
    s.put({ id: 'R-1', status: 'active' });
    s.put({ id: 'R-1', status: 'stopped' });
    expect(s.findBy('status', 'active')).toEqual([]);
    expect(ids(s.findBy('status', 'stopped'))).toEqual(['R-1']);
    // A value that isn't a primitive isn't indexed
    s.put({ id: 'R-2', status: { not: 'a key' } });
    expect(s.findBy('status', 'stopped')).toHaveLength(1);
  });

  it('files a record under a computed index only when it derives a value, and keeps the record as written', () => {
    const s = fresh();
    s.put({ id: 'R-1' });
    s.put({ id: 'R-2', parentId: 'R-1' });
    expect(ids(s.findBy('isRoot', true))).toEqual(['R-1']);
    expect(s.get('R-1')).toEqual({ id: 'R-1' });
    s.put({ id: 'R-1', parentId: 'R-0' });
    expect(s.findBy('isRoot', true)).toEqual([]);
  });

  it('finds records in creation order, newest first in reverse, by ids handed out in the same millisecond', () => {
    const s = fresh();
    const made = Array.from({ length: 50 }, () => s.newId('R'));
    // The ids are written out of order: the index sorts them, not the writes
    for (const id of [...made].reverse()) s.put({ id, parentId: 'P' });
    expect(ids(s.findBy('parentId', 'P'))).toEqual(made);
    expect(ids(s.findBy('parentId', 'P', { reverse: true }))).toEqual([...made].reverse());
  });

  it('pages from a cursor in either direction', () => {
    const s = fresh();
    const made = Array.from({ length: 5 }, () => s.newId('R'));
    for (const id of made) s.put({ id, parentId: 'P' });
    expect(ids(s.findBy('parentId', 'P', { limit: 2 }))).toEqual(made.slice(0, 2));
    expect(ids(s.findBy('parentId', 'P', { limit: 2, cursor: made[1] }))).toEqual(made.slice(2, 4));
    expect(ids(s.findBy('parentId', 'P', { reverse: true, limit: 2, cursor: made[3] }))).toEqual([made[2], made[1]]);
  });

  it('finds nothing for a limit of none', () => {
    const s = fresh();
    s.put({ id: s.newId('R'), parentId: 'P' });
    expect(s.findBy('parentId', 'P', { limit: 0 })).toEqual([]);
  });

  it('is empty after a reset', async () => {
    const s = fresh();
    s.put({ id: 'R-1', parentId: 'P' });
    await s.reset();
    expect(s.get('R-1')).toBeUndefined();
    expect(s.findBy('parentId', 'P')).toEqual([]);
  });
});

describe('openRecordStore', () => {
  // A reset or an import closes the store while its files are replaced, and the app goes on running meanwhile
  it('drops writes and finds nothing while closed, and refuses to copy itself', async () => {
    const s = store();
    s.put({ id: 'R-1', parentId: 'P' });
    s.close();
    expect(s.get('R-1')).toBeUndefined();
    expect(s.findBy('parentId', 'P')).toEqual([]);
    s.put({ id: 'R-2', parentId: 'P' });
    s.delete('R-1');
    await expect(s.copyTo(path.join(path.dirname(dir), 'copy'))).rejects.toThrow('is closed');
    s.reopen();
    expect(ids(s.findBy('parentId', 'P'))).toEqual(['R-1']);
  });

  it('keeps its records across a reopen', async () => {
    const s = store();
    s.put({ id: 'R-1', parentId: 'P' });
    s.reopen();
    expect(s.get('R-1')).toBeDefined();
  });

  it('copies itself into a directory a store opens again', async () => {
    const s = store();
    s.put({ id: 'R-1', parentId: 'P' });
    const copy = path.join(path.dirname(dir), 'copy');
    await s.copyTo(copy);
    expect(ids(store({ at: copy, readOnly: true }).findBy('parentId', 'P'))).toEqual(['R-1']);
  });

  it('opens read-only without writing, and refuses a directory with no store in it', () => {
    store().put({ id: 'R-1', status: 'active' });
    open.splice(0).forEach((opened) => opened.close());
    const readOnly = store({ readOnly: true });
    expect(readOnly.get('R-1')).toBeDefined();
    expect(() => readOnly.put({ id: 'R-2' })).toThrow('read-only');
    expect(() => store({ readOnly: true, at: path.join(root, 'nothing-here') })).toThrow('No record store');
  });

  it('refuses a write once opening its files again has failed, rather than dropping it', () => {
    const s = store();
    s.close();
    // Its files replaced by ones it refuses to open
    fs.rmSync(dir, { recursive: true, force: true });
    openEnvironment(dir).root.close();
    expect(() => s.reopen()).toThrow('are an EARS database, not a record store');
    expect(() => s.put({ id: 'R-1' })).toThrow('opening it again failed');
    expect(s.findBy('status', 'active')).toEqual([]);
  });

  it('refuses an EARS database, and an EARS environment refuses it', () => {
    const ears = path.join(path.dirname(dir), 'ears');
    openEnvironment(ears).root.close();
    expect(() => store({ at: ears })).toThrow('are an EARS database, not a record store');

    store().close();
    expect(() => openEnvironment(dir)).toThrow('are a "records" store, not an EARS database');
  });
});
