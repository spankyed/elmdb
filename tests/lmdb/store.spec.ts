// openLmdbStore: the engine reads and writes the store, so what a unit commits is on disk however the process ends,
// a store opened again on the same path reads it back, and a sync follows once writes pause
import { spawn } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { open as openRaw } from 'lmdb';
import { createEarsEngine, type EARS, type EarsEngine } from '../../src/index.ts';
import { COMPACT_COPIES, LMDB_FORMAT_VERSION, openLmdbStore, SYNC_IDLE_MS, SYNC_INTERVAL_MS, type LmdbStore, type LmdbStoreOptions } from '../../src/lmdb/index.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-lmdb-store-'));
let dbPath: string;
const opened: LmdbStore[] = [];
afterAll(() => fs.rmSync(root, { recursive: true, force: true }));

beforeEach(() => {
  dbPath = path.join(fs.mkdtempSync(path.join(root, 'case-')), 'ears-db');
});
afterEach(() => {
  for (const store of opened.splice(0)) store.close();
});

/** A store at the case's path, and an engine on it */
function openStore(options: Omit<LmdbStoreOptions, 'path'> = {}, at = dbPath): { store: LmdbStore; engine: EarsEngine } {
  const store = openLmdbStore({ path: at, ...options });
  opened.push(store);
  const engine = createEarsEngine({ kv: store.kv, isEntityType: (name) => name === 'Note', isRelationKind: () => true, isRole: () => true });
  return { store, engine };
}
const closeAll = () => { for (const store of opened.splice(0)) store.close(); };
const id = (name: string) => name as EARS.EntityId;

describe('openLmdbStore', () => {
  it('keeps what was committed across a close and a new store on the same path', () => {
    const { engine } = openStore();
    engine.create('Note', { id: id('Note-1'), title: 'kept' }).link('refs', engine.create('Note').id());
    engine.write(id('Note-1')).trash();
    closeAll();
    const { engine: again } = openStore();
    expect(again.getRow(id('Note-1'))).toMatchObject({ title: 'kept', trashedAt: expect.any(Number) });
    expect(again.query('Note').count()).toBe(1);
    expect(again.findRelations({ source: id('Note-1') })).toHaveLength(1);
  });

  it('opens existing files read-only, and refuses a write, a reset, a replacement and a directory with no database', () => {
    openStore().engine.create('Note', { id: id('Note-1'), title: 'there' });
    closeAll();
    const { store, engine } = openStore({ readOnly: true });
    expect(engine.getAttr(id('Note-1'), 'title' as EARS.AttrKind)).toBe('there');
    expect(() => engine.create('Note', { title: 'refused' })).toThrow();
    expect(() => store.reset()).toThrow('read-only');
    expect(() => store.replaceWith(dbPath)).toThrow('read-only');
    expect(() => openLmdbStore({ path: path.join(root, 'nowhere'), readOnly: true })).toThrow('No LMDB database');
  });

  it('answers an index lookup on a read-only store whose index was never built, from the type\'s rows', () => {
    // Indexes are built on their first read, which a read-only store can't write: a converted database has none built
    const indexing = (store: LmdbStore) => createEarsEngine({ kv: store.kv, isEntityType: (name) => name === 'Note', isRelationKind: () => true, isRole: () => true, rowRules: () => ({ required: [], indexes: ['status'] }) });
    const writer = openLmdbStore({ path: dbPath });
    indexing(writer).create('Note', { id: id('Note-1'), status: 'open' }).grant('pinned');
    writer.close();
    const reader = openLmdbStore({ path: dbPath, readOnly: true });
    opened.push(reader);
    const engine = indexing(reader);
    expect(engine.query('Note').where('status', 'open').ids()).toEqual(['Note-1']);
    expect(engine.query('Note').withRole('pinned').ids()).toEqual(['Note-1']);
  });

  it('resets to empty, and replaces its entries with another database\'s in one unit', async () => {
    const { store, engine } = openStore();
    engine.create('Note', { id: id('Note-old'), title: 'old' });
    const other = path.join(path.dirname(dbPath), 'other');
    const { store: source, engine: sourceEngine } = openStore({}, other);
    sourceEngine.create('Note', { id: id('Note-new'), title: 'new' });
    source.close();
    store.replaceWith(other);
    expect(engine.query('Note').ids()).toEqual(['Note-new']);
    store.reset();
    expect(engine.query('Note').ids()).toEqual([]);
    engine.create('Note', { title: 'after a reset' });
    expect(engine.query('Note').count()).toBe(1);
  });

  // Compaction is off while lmdb aborts emptying a compacted copy (COMPACT_COPIES): this runs once it is on
  it.skipIf(!COMPACT_COPIES)('copies the database without the pages its deletes freed', async () => {
    const { store, engine } = openStore();
    engine.transact(() => { for (let i = 0; i < 200; i++) engine.create('Note', { id: id(`Note-${i}`), title: 'x'.repeat(10_000) }); });
    engine.transact(() => { for (let i = 1; i < 200; i++) engine.write(id(`Note-${i}`)).destroy(); });
    const copy = path.join(path.dirname(dbPath), 'compacted');
    await store.copyTo(copy);
    expect(fs.statSync(path.join(copy, 'data.mdb')).size).toBeLessThan(fs.statSync(path.join(dbPath, 'data.mdb')).size / 10);
    const { engine: copied } = openStore({ readOnly: true }, copy);
    expect(copied.query('Note').ids()).toEqual(['Note-0']);
  });

  it('copies one moment of the database, which opens on its own', async () => {
    const { store, engine } = openStore();
    engine.create('Note', { id: id('Note-1'), title: 'before' });
    const copy = path.join(path.dirname(dbPath), 'copy');
    const copying = store.copyTo(copy);
    engine.write(id('Note-1')).set('title', 'after');
    await copying;
    // One moment's: the copy's read transaction is taken when the copy starts, which may follow the write
    const { engine: copied } = openStore({ readOnly: true }, copy);
    expect(['before', 'after']).toContain(copied.getAttr(id('Note-1'), 'title' as EARS.AttrKind));
    expect(copied.query('Note').count()).toBe(1);
  });

  it('refuses files in an older format, naming the conversion, and leaves them as they were', () => {
    fs.mkdirSync(dbPath, { recursive: true });
    const env = openRaw({ path: dbPath, maxDbs: 8 });
    env.openDB({ name: 'meta', encoding: 'json' }).putSync('format', 3);
    env.openDB({ name: 'attrs', encoding: 'json' }).putSync('title\x1FNote-1', { t: 'string', v: 'old' });
    env.close();
    expect(() => openLmdbStore({ path: dbPath })).toThrow(`is in storage format 3, but this version reads format ${LMDB_FORMAT_VERSION}: convert a copy of it with scripts/ears-convert.ts`);
  });
});

describe('durability', () => {
  // The store's timers are faked, so when a sync starts is the spec's to say; the sync itself is LMDB's own
  const fakeTimers = () => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
  afterEach(() => { vi.useRealTimers(); });

  it('syncs once writes pause: a burst of units, one sync, SYNC_IDLE_MS after the last', () => {
    fakeTimers();
    const { store, engine } = openStore();
    // Five commits, each before the last one's pause is up, all inside the cap
    for (let i = 0; i < 5; i++) {
      engine.create('Note', { title: `note ${i}` });
      vi.advanceTimersByTime(SYNC_IDLE_MS / 2);
    }
    expect(store.syncs, 'each commit pushes the sync back').toBe(0);
    vi.advanceTimersByTime(SYNC_IDLE_MS / 2);
    expect(store.syncs).toBe(1);
    vi.advanceTimersByTime(SYNC_INTERVAL_MS);
    expect(store.syncs, 'nothing is owed after it').toBe(1);
  });

  it('syncs a steady stream of writes SYNC_INTERVAL_MS after its first unsynced commit', () => {
    fakeTimers();
    const { store, engine } = openStore();
    const step = SYNC_IDLE_MS / 2;
    for (let elapsed = 0; elapsed < SYNC_INTERVAL_MS - step; elapsed += step) {
      engine.create('Note', { title: `at ${elapsed}` });
      vi.advanceTimersByTime(step);
    }
    expect(store.syncs).toBe(0);
    engine.create('Note', { title: 'the last before the cap' });
    vi.advanceTimersByTime(step);
    expect(store.syncs).toBe(1);
  });

  it('syncs at once for durable(), and not when nothing is owed', async () => {
    const { store, engine } = openStore();
    await store.durable();
    expect(store.syncs).toBe(0);
    engine.create('Note', { title: 'must be on disk' });
    await store.durable();
    expect(store.syncs).toBe(1);
    await store.durable();
    expect(store.syncs).toBe(1);
  });

  it('commits a durable unit with its sync, owing none after it', async () => {
    const { store, engine } = openStore({ syncIntervalMs: 60_000 });
    engine.transact(() => engine.create('Note', { title: 'backed up' }), { durable: true });
    await store.durable();
    expect(store.syncs).toBe(0);
  });

  it('keeps a write its process committed, though the process is killed the moment the write returns', async () => {
    const child = path.join(path.dirname(dbPath), 'writer.mts');
    const source = (file: string) => pathToFileURL(fileURLToPath(new URL(`../../src/${file}`, import.meta.url))).href;
    fs.writeFileSync(child, [
      `import { createEarsEngine } from ${JSON.stringify(source('index.ts'))};`,
      `import { openLmdbStore } from ${JSON.stringify(source('lmdb/index.ts'))};`,
      `const store = openLmdbStore({ path: process.argv[2], syncIntervalMs: 60_000 });`,
      `const engine = createEarsEngine({ kv: store.kv, isEntityType: () => true, isRelationKind: () => true, isRole: () => true });`,
      `engine.create('Note', { id: 'Note-killed', title: 'committed' });`,
      `process.stdout.write('written\\n');`,
      `setInterval(() => {}, 1000);`,
    ].join('\n'));
    const writer = spawn(process.execPath, ['--import', 'tsx', child, dbPath], { stdio: ['ignore', 'pipe', 'pipe'] });
    let errors = '';
    writer.stderr.on('data', (chunk) => { errors += String(chunk); });
    await new Promise<void>((resolve, reject) => {
      writer.stdout.on('data', (chunk) => { if (String(chunk).includes('written')) resolve(); });
      writer.on('exit', (code) => reject(new Error(`the writer exited (${code}) before writing: ${errors}`)));
    });
    writer.kill('SIGKILL');
    await new Promise((resolve) => writer.on('exit', resolve));
    const { engine } = openStore({ readOnly: true });
    expect(engine.getAttr(id('Note-killed'), 'title' as EARS.AttrKind)).toBe('committed');
  });
});

describe('reads', () => {
  it('a thousand queries that stop early or throw leave the environment\'s readers where they were', () => {
    const { store, engine } = openStore();
    engine.transact(() => { for (let i = 0; i < 20; i++) engine.create('Note', { title: `note ${i}` }); });
    const before = store.readers();
    for (let i = 0; i < 1_000; i++) {
      engine.query('Note').first();
      expect(() => engine.query('Note').where('title', () => { throw new Error('predicate'); }).ids()).toThrow('predicate');
      if (i % 100 === 0) engine.create('Note', { title: 'between' });
    }
    expect(store.readers()).toBeLessThanOrEqual(Math.max(before, 1));
  });
});
