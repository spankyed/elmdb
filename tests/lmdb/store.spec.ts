// openLmdbStore: the engine's writes reach LMDB through store.sink, and a store opened again on the same
// path hydrates them back
//
// @slow: 22 cases, each opening a real LMDB environment in a temp dir of its own, at ~150ms apiece
// There is no outlier to extract — measured, the cases run 104-346ms and the median is the profile — so
// what would shorten this is opening fewer environments, which is the subject. `beforeEach` mkdtemps a
// directory per case because what is being checked is the state *on disk* across a close, a reopen and a
// reset; several cases open the same path twice to prove hydration, and sharing one environment between
// them would delete the thing under test.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterAll, afterEach, beforeEach, describe, expect, it } from 'vitest';
import { createEarsEngine, type EARS, type EarsEngine } from '../../src/index.ts';
import { open as openEnv } from 'lmdb';
import { LMDB_FORMAT_VERSION, convertToCurrentFormat, openLmdbStore, type LmdbStore, type LmdbStoreOptions } from '../../src/lmdb/index.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-lmdb-store-'));
let dbPath: string;
const open: LmdbStore[] = [];
let engine: EarsEngine;

/** A store and a new engine persisting to it, installed, as an app's composition opens them */
function openStore(options: Pick<LmdbStoreOptions, 'readOnly' | 'log'> = {}): LmdbStore {
  const store = openLmdbStore({ path: dbPath, engine: () => engine.admin, ...options });
  engine = createEarsEngine({ persistence: store.sink, isEntityType: (name) => name === 'Note', isRelationKind: () => true, isRole: () => true });
  open.push(store);
  return store;
}
const getAttr = (entity: EARS.EntityId, kind: string) => engine.getRow(entity)?.[kind];

/** The adapter flushes in a microtask */
const flushed = () => new Promise<void>((resolve) => setImmediate(resolve));
const id = (name: string) => name as EARS.EntityId;

beforeEach(() => {
  const dir = fs.mkdtempSync(path.join(root, 'case-'));
  dbPath = path.join(dir, 'ears-db');
});
afterEach(() => {
  for (const store of open.splice(0)) store.close();
});
afterAll(() => fs.rmSync(root, { recursive: true, force: true }));

describe('openLmdbStore', () => {
  it('stores nothing of a unit that throws, a destroy included, and all of one that returns', async () => {
    const first = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    await flushed();

    expect(() => engine.transact(() => {
      engine.write(id('Note-1')).destroy();
      engine.create('Note', { id: id('Note-2') }).set('title', 'never');
      throw new Error('stop');
    })).toThrow('stop');
    engine.transact(() => { engine.create('Note', { id: id('Note-3') }).set('title', 'committed'); });
    await flushed();
    first.close();

    const second = openStore();
    await second.hydrate();
    expect(getAttr(id('Note-1'), 'title')).toBe('kept');
    expect(getAttr(id('Note-2'), 'title')).toBeUndefined();
    expect(getAttr(id('Note-3'), 'title')).toBe('committed');
  });

  it('persists writes across a close and a new store on the same path', async () => {
    const first = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    engine.create('Note', { id: id('Note-2') }).set('title', 'mentioned');
    engine.write(id('Note-1')).link('mentions', id('Note-2'));
    await flushed();
    first.close();

    // A new engine: nothing in memory
    const second = openStore();
    expect(engine.admin.getEntitiesOfType('Relation')).toEqual([]);
    await second.hydrate();
    expect(getAttr(id('Note-1'), 'title')).toBe('kept');
    expect(engine.admin.getEntitiesOfType('Relation')).toHaveLength(1);
  });

  it('hydrates only the entities its entity table holds, whatever attribute rows were left without one', async () => {
    const first = openStore();
    engine.create('Note', { id: id('Note-kept') }).set('title', 'kept');
    engine.create('Note', { id: id('Note-lost') }).set('title', 'its row was lost');
    await flushed();
    first.close();
    // What a flush that fell back to row by row can leave: the attributes stored, the entity's row not
    const root = openEnv({ path: dbPath, maxDbs: 8, compression: true });
    root.openDB({ name: 'entities', encoding: 'json' }).removeSync('Note-lost');
    root.close();

    const lines: string[] = [];
    await openStore({ log: (line) => lines.push(line) }).hydrate();
    expect(engine.query('Note').ids()).toEqual([id('Note-kept')]);
    expect(getAttr(id('Note-lost'), 'title')).toBeUndefined();
    expect(lines).toContainEqual(expect.stringMatching(/^\[Hydrate\] Skipped \d+ attribute rows of entities with no entity row$/));
  });

  it('reads back every value as it was written, a one-element list, a grown list and roles included', async () => {
    const first = openStore();
    const note = engine.create('Note', { id: id('Note-1') })
      .set('tags', ['only']).append('grown', 'a').append('grown', 'b').set('meta', { k: [1] }).set('nothing', null)
      .grant('current').id();
    await flushed();
    const written = engine.getRow(note);
    first.close();

    await openStore().hydrate();
    expect(engine.getRow(note)).toEqual(written);
    expect(written).toMatchObject({ tags: ['only'], grown: ['a', 'b'], meta: { k: [1] }, nothing: null, role: ['current'] });
  });

  it('keeps an entity in the trash across a close: hydrated, it is still out of every query', async () => {
    const first = openStore();
    const binned = engine.create('Note', { id: id('Note-binned') }).trash(9).id();
    engine.create('Note', { id: id('Note-kept') });
    await flushed();
    first.close();

    await openStore().hydrate();
    expect(engine.query('Note').ids()).toEqual([id('Note-kept')]);
    expect(engine.query.trashed('Note').pickAll()).toEqual([expect.objectContaining({ id: binned, trashedAt: 9 })]);
  });

  it('keeps one sink across a reopen, and drops writes while closed', async () => {
    const store = openStore();
    const { sink } = store;
    store.close();
    expect(store.isOpen()).toBe(false);
    expect(() => store.env).toThrow('The LMDB store is closed');
    engine.create('Note', { id: id('Note-dropped') }).set('title', 'lost');

    store.reopen();
    expect(store.isOpen()).toBe(true);
    expect(store.sink).toBe(sink);
    engine.create('Note', { id: id('Note-after') }).set('title', 'saved');
    await flushed();
    expect(store.read().getAttr('Note-after', 'title')).toBe('saved');
    expect(store.read().getEntityMeta('Note-dropped')).toBeNull();
  });

  it('reset deletes the files and opens the store empty', async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'gone');
    await flushed();
    await store.reset();
    expect(store.isOpen()).toBe(true);
    expect(store.read().getEntityMeta('Note-1')).toBeNull();
    // The engine's memory is the caller's
    expect(getAttr(id('Note-1'), 'title')).toBe('gone');
  });

  it('writes what the engine wrote while a reset had the store closed to the new files', async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'gone');
    await flushed();

    const resetting = store.reset();
    // A system still running writes while the files are being replaced
    expect(store.isOpen()).toBe(false);
    engine.create('Note', { id: id('Note-2') }).set('title', 'written during the reset');
    await resetting;
    await flushed();

    expect(store.read().getEntityMeta('Note-1')).toBeNull();
    expect(store.read().getAttr('Note-2', 'title')).toBe('written during the reset');
    // A store closed any other way still drops writes
    store.close();
    engine.create('Note', { id: id('Note-3') }).set('title', 'dropped');
    store.reopen();
    await flushed();
    expect(store.read().getEntityMeta('Note-3')).toBeNull();
  });

  it("updates a relation it didn't see written in place, keeping when it was stored", async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'a');
    engine.create('Note', { id: id('Note-2') }).set('title', 'b');
    engine.create('Note', { id: id('Note-3') }).set('title', 'c');
    const rel = engine.admin.addRelation(id('Note-1'), 'mentions', id('Note-2'));
    await flushed();
    const { createdAt } = store.env.relations.get(rel);

    store.reopen();
    await new Promise((resolve) => setTimeout(resolve, 5));
    engine.admin.updateRelation(rel, { target: id('Note-3'), info: 'why' });
    await flushed();
    expect(store.env.relations.get(rel)).toEqual({ kind: 'mentions', src: 'Note-1', tgt: 'Note-3', info: 'why', createdAt });
  });

  it('keeps an entity destroyed right after its writes, and a removed relation, out of the files', async () => {
    const first = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    engine.create('Note', { id: id('Note-2') }).set('title', 'ghost');
    const rel = engine.admin.addRelation(id('Note-1'), 'mentions', id('Note-1'));
    await flushed();
    // A relation is its row in the relations table, and no entity row
    expect(first.env.relations.get(rel)).toBeTruthy();
    expect(first.read().getEntityMeta(rel)).toBeNull();
    // In the same synchronous block as its writes, before they're flushed
    engine.write(id('Note-2')).set('title', 'ghost again');
    engine.write(id('Note-2')).destroy();
    engine.write(rel).destroy();
    await flushed();
    expect(first.read().getEntityMeta('Note-2')).toBeNull();
    expect(first.env.relations.get(rel)).toBeUndefined();
    first.close();

    const second = openStore();
    await second.hydrate();
    expect(getAttr(id('Note-1'), 'title')).toBe('kept');
    expect(engine.getRow(id('Note-2'))).toBeUndefined();
    expect(engine.admin.getEntitiesOfType('Note')).toEqual(['Note-1']);
    expect(engine.admin.getEntitiesOfType('Relation')).toEqual([]);
  });

  it('opens existing files read-only: it hydrates them, and refuses writes and a reset', async () => {
    const first = openStore();
    engine.create('Note', { id: id('Note-2') }).set('title', 'mentioned');
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept').link('mentions', id('Note-2'));
    await flushed();
    first.close();

    const lines: string[] = [];
    const readOnly = openStore({ readOnly: true, log: (line) => lines.push(line) });
    expect(readOnly.readOnly).toBe(true);
    expect(readOnly.path).toBe(dbPath);
    await readOnly.hydrate();
    expect(getAttr(id('Note-1'), 'title')).toBe('kept');
    expect(engine.admin.getEntitiesOfType('Relation')).toHaveLength(1);
    expect(lines).toContainEqual(expect.stringMatching(/^\[LMDB\] Hydrated \d+ attributes, 1 relations$/));
    expect(() => engine.write(id('Note-1')).set('title', 'changed')).toThrow('is open read-only');
    await expect(readOnly.reset()).rejects.toThrow('is open read-only');
    expect(readOnly.close()).toEqual({ errorCount: 0, lastError: null });
    expect(lines).toContain('[LMDB] Environment closed successfully');

    const reopened = openStore();
    expect(reopened.read().getAttr('Note-1', 'title')).toBe('kept');
  });

  it('reports writes lost while it was closed for a reopen or a reset, not only this close\'s', async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    await flushed();
    // JSON can't encode a BigInt: the write fails when the store closes to reopen
    engine.write(id('Note-1')).set('count', 1n);
    store.reopen();
    expect(store.close()).toMatchObject({ errorCount: 1, lastError: { op: 'final flush' } });
    // Reported once
    expect(store.close()).toEqual({ errorCount: 0, lastError: null });

    const resetting = openStore();
    engine.create('Note', { id: id('Note-2') }).set('count', 2n);
    await resetting.reset();
    expect(resetting.close()).toMatchObject({ errorCount: 1 });
  });

  it('opens no database where none exists when read-only', () => {
    expect(() => openStore({ readOnly: true })).toThrow();
    expect(fs.existsSync(dbPath)).toBe(false);
  });

  it('reports a failed final flush from close, and nothing for a clean or repeated close', async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'fine');
    await flushed();
    // JSON can't encode a BigInt: the write fails when close flushes it
    engine.write(id('Note-1')).set('count', 1n);
    const stats = store.close();
    expect(stats.errorCount).toBe(1);
    expect(stats.lastError).toMatchObject({ op: 'final flush' });
    expect(store.close()).toEqual({ errorCount: 0, lastError: null });

    const clean = openStore();
    engine.create('Note', { id: id('Note-2') }).set('title', 'fine');
    expect(clean.close()).toEqual({ errorCount: 0, lastError: null });
  });
});

describe('the storage format', () => {
  /** The environment's own records, opened without the format check */
  function meta(basePath: string) {
    const root = openEnv({ path: basePath, maxDbs: 8, compression: true });
    return { db: root.openDB({ name: 'meta', encoding: 'json' }), close: () => root.close() };
  }

  function storedFormat(basePath: string): unknown {
    const { db, close } = meta(basePath);
    try {
      return db.get('format');
    } finally {
      close();
    }
  }

  /** Records `format`, or, without one, the files of a version that recorded none */
  function writeFormat(basePath: string, format?: unknown): void {
    const { db, close } = meta(basePath);
    try {
      if (format === undefined) db.removeSync('format');
      else db.putSync('format', format);
    } finally {
      close();
    }
  }

  // A store that can't open its files again has nowhere to keep a write: dropping writes silently would leave the
  // app running and losing everything the user does
  it('fails a write after opening the files again failed, and takes them once it works', async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    await flushed();

    // The files the reopen finds are in a format this version doesn't read
    writeFormat(dbPath, LMDB_FORMAT_VERSION + 1);
    expect(() => store.reopen()).toThrow(String(LMDB_FORMAT_VERSION + 1));

    expect(() => engine.create('Note', { id: id('Note-2') }).set('title', 'lost')).toThrow(/opening it again failed/);

    writeFormat(dbPath, LMDB_FORMAT_VERSION);
    store.reopen();
    engine.create('Note', { id: id('Note-3') }).set('title', 'taken');
    await flushed();
    store.close();

    const reopened = openStore();
    await reopened.hydrate();
    expect(getAttr(id('Note-3'), 'title')).toBe('taken');
    expect(getAttr(id('Note-2'), 'title')).toBeUndefined();
  });

  it('is recorded in a database this version writes', async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    await flushed();
    store.close();
    expect(storedFormat(dbPath)).toBe(LMDB_FORMAT_VERSION);
  });

  it('refuses files that predate it, which are format 1, until they are converted', async () => {
    openStore().close();
    // Files from before the format was recorded, holding data: an attribute row per list index
    writeFormat(dbPath);
    const root = openEnv({ path: dbPath, maxDbs: 8, compression: true });
    root.openDB({ name: 'entities', encoding: 'json' }).putSync('Note-1', { type: 'Note', createdAt: 1 });
    root.openDB({ name: 'attrs', encoding: 'json' }).putSync('title\x1FNote-1\x1F0', { t: 'string', v: 'kept' });
    root.close();

    const format1 = new RegExp(`storage format 1, but this version reads format ${LMDB_FORMAT_VERSION}: convert a copy of it with scripts/ears-convert\\.ts`);
    expect(() => openStore({ readOnly: true, log: () => {} })).toThrow(format1);
    expect(() => openStore()).toThrow(format1);
    expect(storedFormat(dbPath)).toBeUndefined();

    convertToCurrentFormat(dbPath);
    expect(storedFormat(dbPath)).toBe(LMDB_FORMAT_VERSION);
    const converted = openStore({ log: () => {} });
    await converted.hydrate();
    expect(getAttr(id('Note-1'), 'title')).toBe('kept');
  });

  it('records the format in new, empty files, which hold nothing to misread', () => {
    // A version that recorded no format, and wrote nothing
    const root = openEnv({ path: dbPath, maxDbs: 8, compression: true });
    for (const name of ['entities', 'attrs', 'relations']) root.openDB({ name, encoding: 'json' });
    root.close();
    openStore().close();
    expect(storedFormat(dbPath)).toBe(LMDB_FORMAT_VERSION);
  });

  it('refuses a directory with no database in it when read-only', () => {
    fs.mkdirSync(dbPath, { recursive: true });
    expect(() => openStore({ readOnly: true })).toThrow(`No LMDB database at ${dbPath}`);
  });

  it('refuses another format, saying which, and leaves the files closed', async () => {
    openStore().close();
    writeFormat(dbPath, LMDB_FORMAT_VERSION + 1);

    const newer = new RegExp(`storage format ${LMDB_FORMAT_VERSION + 1}, but this version reads format ${LMDB_FORMAT_VERSION}: it was written by a newer apack`);
    expect(() => openStore()).toThrow(newer);
    expect(() => openStore({ readOnly: true })).toThrow(newer);

    writeFormat(dbPath, 'nonsense');
    expect(() => openStore()).toThrow(new RegExp(`storage format "nonsense", but this version reads format ${LMDB_FORMAT_VERSION}$`));

    // The environment it opened is closed, so the files can be replaced and opened again
    fs.rmSync(dbPath, { recursive: true, force: true });
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'after');
    await flushed();
    expect(store.read().getAttr('Note-1', 'title')).toBe('after');
    expect(storedFormat(dbPath)).toBe(LMDB_FORMAT_VERSION);
  });
});

describe('snapshot', () => {
  /** Rows in a copied database, read straight from its files */
  function rowsIn(dir: string): Array<{ key: unknown; value: unknown }> {
    const root = openEnv({ path: dir, maxDbs: 8, compression: true, readOnly: true });
    try {
      return [...root.openDB({ name: 'attrs', encoding: 'json' }).getRange()];
    } finally {
      root.close();
    }
  }

  it('copies the database into a directory of its own, readable on its own', async () => {
    const store = openStore({ log: () => {} });
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    await flushed();

    const target = path.join(fs.mkdtempSync(path.join(root, 'snap-')), 'lmdb');
    await store.copyTo(target);

    // The layout a backup folder holds: the directory is made, with data.mdb in it and no lock file
    expect(fs.readdirSync(target)).toEqual(['data.mdb']);
    expect(rowsIn(target).some(({ value }) => JSON.stringify(value).includes('kept'))).toBe(true);
  });

  it('holds one moment of the database, though writes land while it runs', async () => {
    const store = openStore({ log: () => {} });
    for (let n = 0; n < 400; n++) engine.create('Note', { id: id(`Note-${n}`) }).set('title', `before ${n}`);
    await flushed();

    // Writes carry on across the copy, as they would while the app runs
    const target = path.join(fs.mkdtempSync(path.join(root, 'snap-')), 'lmdb');
    const copying = store.copyTo(target);
    for (let n = 400; n < 800; n++) engine.create('Note', { id: id(`Note-${n}`) }).set('title', `during ${n}`);
    await copying;
    await flushed();

    // Whatever the copy caught, it is a database that opens and reads: never a half-made commit
    const rows = rowsIn(target);
    const titles = rows.map(({ value }) => JSON.stringify(value));
    expect(titles.filter((t) => t.includes('before')).length).toBe(400);
    // And the store itself is untouched by having been copied
    expect(getAttr(id('Note-799'), 'title')).toBe('during 799');
  });

  it('refuses to copy a closed store', async () => {
    const store = openStore({ log: () => {} });
    store.close();
    const target = path.join(fs.mkdtempSync(path.join(root, 'snap-')), 'lmdb');
    await expect(store.copyTo(target)).rejects.toThrow('The LMDB store is closed');
  });
});
