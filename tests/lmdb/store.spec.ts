// openLmdbStore: the engine's writes reach LMDB through store.sink, and a store opened again on the same
// paths hydrates them back
//
// @slow: 24 cases, each opening a real LMDB environment in a temp dir of its own, at ~150ms apiece
// There is no outlier to extract — measured, the cases run 104-346ms and the median is the profile — so
// what would shorten this is opening fewer environments, which is the subject. `beforeEach` mkdtemps a
// directory per case because what is being checked is the state *on disk* across a close, a reopen and a
// reset; several cases open the same paths twice to prove hydration, and sharing one environment between
// them would delete the thing under test.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterAll, afterEach, beforeEach, describe, expect, it } from 'vitest';
import { createEarsEngine, makePolicy, type EARS, type EarsEngine } from '../../src/index.ts';
import { open as openEnv } from 'lmdb';
import { LMDB_FORMAT_VERSION, convertToCurrentFormat, openLmdbStore, type LmdbStore, type LmdbStoreOptions } from '../../src/lmdb/index.ts';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-lmdb-store-'));
const policy = makePolicy({ excludedEntityTypes: new Set(['Trace']) });
let paths: { primary: string; volatileBackup: string };
const open: LmdbStore[] = [];
let engine: EarsEngine;

/** A store and a new engine persisting to it, installed, as an app's composition opens them */
function openStore(options: Pick<LmdbStoreOptions, 'readOnly' | 'log'> = {}): LmdbStore {
  const store = openLmdbStore({ paths, policy, engine: () => engine.admin, ...options });
  engine = createEarsEngine({ persistence: store.sink, isEntityType: (name) => name === 'Note' || name === 'Trace', isRelationKind: () => true, isRole: () => true });
  open.push(store);
  return store;
}
const getAttr = (entity: EARS.EntityId, kind: string) => engine.getAttr(entity, kind);

/** The adapter flushes in a microtask */
const flushed = () => new Promise<void>((resolve) => setImmediate(resolve));
const id = (name: string) => name as EARS.EntityId;

beforeEach(() => {
  const dir = fs.mkdtempSync(path.join(root, 'case-'));
  paths = { primary: path.join(dir, 'ears-db'), volatileBackup: path.join(dir, 'ears-trace') };
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
    expect(getAttr(id('Note-2'), 'title')).toBeNull();
    expect(getAttr(id('Note-3'), 'title')).toBe('committed');
  });

  it('persists writes across a close and a new store on the same paths', async () => {
    const first = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    engine.create('Note', { id: id('Note-2') }).set('title', 'mentioned');
    engine.create('Trace', { id: id('Trace-1') }).set('step', 'volatile');
    engine.write(id('Note-1')).link('mentions', id('Note-2'));
    await flushed();
    first.close();

    // A new engine: nothing in memory
    const second = openStore();
    expect(engine.admin.getEntitiesOfType('Relation')).toEqual([]);
    await second.hydrate();
    expect(getAttr(id('Note-1'), 'title')).toBe('kept');
    expect(engine.admin.getEntitiesOfType('Relation')).toHaveLength(1);
    // Only the primary partition is hydrated; the volatile one is read directly
    expect(getAttr(id('Trace-1'), 'step')).toBeNull();
    expect(second.read('volatileBackup').getAttr('step', 'Trace-1')).toBe('volatile');

    await second.hydrate({ includeVolatile: true });
    expect(getAttr(id('Trace-1'), 'step')).toBe('volatile');
  });

  it('reads back every value as it was written, a one-element list, a grown list and roles included', async () => {
    const first = openStore();
    const note = engine.create('Note', { id: id('Note-1') })
      .set('tags', ['only']).add('grown', 'a').add('grown', 'b').set('meta', { k: [1] }).set('nothing', null)
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
    expect(() => store.envs).toThrow('The LMDB store is closed');
    engine.create('Note', { id: id('Note-dropped') }).set('title', 'lost');

    store.reopen();
    expect(store.isOpen()).toBe(true);
    expect(store.sink).toBe(sink);
    engine.create('Note', { id: id('Note-after') }).set('title', 'saved');
    await flushed();
    expect(store.read('primary').getAttr('title', 'Note-after')).toBe('saved');
    expect(store.read('primary').getEntityMeta('Note-dropped')).toBeNull();
  });

  it('reset deletes the files and opens the store empty', async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'gone');
    await flushed();
    await store.reset();
    expect(store.isOpen()).toBe(true);
    expect(store.read('primary').getEntityMeta('Note-1')).toBeNull();
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

    expect(store.read('primary').getEntityMeta('Note-1')).toBeNull();
    expect(store.read('primary').getAttr('title', 'Note-2')).toBe('written during the reset');
    // A store closed any other way still drops writes
    store.close();
    engine.create('Note', { id: id('Note-3') }).set('title', 'dropped');
    store.reopen();
    await flushed();
    expect(store.read('primary').getEntityMeta('Note-3')).toBeNull();
  });

  it("keeps a run's history link when the entity it ran is destroyed, and removes it with the run or an unlink", async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'ran');
    engine.create('Note', { id: id('Note-2') }).set('title', 'ran too');
    engine.create('Trace', { id: id('Trace-1') }).set('step', 'run');
    engine.create('Trace', { id: id('Trace-2') }).set('step', 'run');
    engine.create('Trace', { id: id('Trace-3') }).set('step', 'run');
    const kept = engine.admin.addRelation(id('Trace-1'), 'instance_of', id('Note-1'));
    const withRun = engine.admin.addRelation(id('Trace-2'), 'instance_of', id('Note-2'));
    const unlinked = engine.admin.addRelation(id('Trace-3'), 'instance_of', id('Note-2'));
    const between = engine.admin.addRelation(id('Note-1'), 'mentions', id('Note-2'));
    await flushed();
    const stored = (rel: EARS.EntityId) => store.envs.volatileBackup.relations.get(rel) ?? store.envs.primary.relations.get(rel);

    engine.write(id('Note-1')).destroy();
    engine.write(id('Trace-2')).destroy();
    engine.removeRelationById(unlinked);
    await flushed();

    // The run's record still says which node it ran; the node and its own relations are gone
    expect(stored(kept)).toMatchObject({ kind: 'instance_of', src: 'Trace-1', tgt: 'Note-1' });
    expect(stored(between)).toBeUndefined();
    expect(store.read('primary').getEntityMeta('Note-1')).toBeNull();
    expect(stored(withRun)).toBeUndefined();
    expect(stored(unlinked)).toBeUndefined();
    // Memory drops it with the node
    expect(engine.findRelations({ sourceEntity: id('Trace-1') })).toEqual([]);
  });

  it("keeps a history link it didn't see written when the other end is destroyed", async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'ran');
    const rel = engine.admin.addRelation(id('Trace-1'), 'instance_of', id('Note-1'));
    await flushed();
    store.reopen();
    engine.write(id('Note-1')).destroy();
    await flushed();
    expect(store.envs.volatileBackup.relations.get(rel)).toMatchObject({ src: 'Trace-1', tgt: 'Note-1' });
  });

  it("removes a relation it didn't see written, wherever it is", async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'a');
    const rel = engine.admin.addRelation(id('Note-1'), 'mentions', id('Trace-1'));
    await flushed();
    expect(store.envs.volatileBackup.relations.get(rel)).toMatchObject({ kind: 'mentions', src: 'Note-1', tgt: 'Trace-1' });
    // A reopened store's sink has no relation cache
    store.reopen();
    engine.removeRelationById(rel);
    await flushed();
    expect(store.envs.volatileBackup.relations.get(rel)).toBeUndefined();
  });

  it("moves a relation it didn't see written to its new ends' partition, by the engine's details", async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'a');
    engine.create('Note', { id: id('Note-2') }).set('title', 'b');
    engine.create('Trace', { id: id('Trace-1') }).set('step', 's');
    const rel = engine.admin.addRelation(id('Note-1'), 'mentions', id('Note-2'), { why: 'x' });
    await flushed();
    expect(store.envs.primary.relations.get(rel)).toMatchObject({ src: 'Note-1', tgt: 'Note-2' });

    store.reopen();
    engine.admin.updateRelation(rel, { targetEntity: id('Trace-1') });
    await flushed();
    expect(store.envs.primary.relations.get(rel)).toBeUndefined();
    expect(store.envs.volatileBackup.relations.get(rel)).toMatchObject({ kind: 'mentions', src: 'Note-1', tgt: 'Trace-1', info: { why: 'x' } });
  });

  it("updates a relation it didn't see written in place when its partition doesn't change", async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'a');
    engine.create('Note', { id: id('Note-2') }).set('title', 'b');
    engine.create('Note', { id: id('Note-3') }).set('title', 'c');
    const rel = engine.admin.addRelation(id('Note-1'), 'mentions', id('Note-2'));
    await flushed();
    const { createdAt } = store.envs.primary.relations.get(rel);

    store.reopen();
    await new Promise((resolve) => setTimeout(resolve, 5));
    engine.admin.updateRelation(rel, { targetEntity: id('Note-3'), info: 'why' });
    await flushed();
    expect(store.envs.primary.relations.get(rel)).toEqual({ kind: 'mentions', src: 'Note-1', tgt: 'Note-3', info: 'why', createdAt });
    expect(store.envs.volatileBackup.relations.get(rel)).toBeUndefined();
  });

  it('keeps an entity destroyed right after its writes, and a removed relation, out of the files', async () => {
    const first = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    engine.create('Note', { id: id('Note-2') }).set('title', 'ghost');
    const rel = engine.admin.addRelation(id('Note-1'), 'mentions', id('Note-1'));
    await flushed();
    // A relation is its row in the relations table, and no entity row
    expect(first.envs.primary.relations.get(rel)).toBeTruthy();
    expect(first.read('primary').getEntityMeta(rel)).toBeNull();
    // In the same synchronous block as its writes, before they're flushed
    engine.write(id('Note-2')).set('title', 'ghost again');
    engine.write(id('Note-2')).destroy();
    engine.removeRelationById(rel);
    await flushed();
    expect(first.read('primary').getEntityMeta('Note-2')).toBeNull();
    expect(first.envs.primary.relations.get(rel)).toBeUndefined();
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
    expect(readOnly.paths).toEqual(paths);
    await readOnly.hydrate();
    expect(getAttr(id('Note-1'), 'title')).toBe('kept');
    expect(engine.admin.getEntitiesOfType('Relation')).toHaveLength(1);
    expect(lines).toContain('[LMDB] Hydrating partitions: primary');
    expect(() => engine.write(id('Note-1')).set('title', 'changed')).toThrow('is open read-only');
    await expect(readOnly.reset()).rejects.toThrow('is open read-only');
    expect(readOnly.close()).toEqual({ errorCount: 0, lastError: null });
    expect(lines).toContain('[LMDB] Environment closed successfully');

    const reopened = openStore();
    expect(reopened.read('primary').getAttr('title', 'Note-1')).toBe('kept');
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
    expect(fs.existsSync(paths.primary)).toBe(false);
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
    writeFormat(paths.volatileBackup, LMDB_FORMAT_VERSION + 1);
    expect(() => store.reopen()).toThrow(String(LMDB_FORMAT_VERSION + 1));

    expect(() => engine.create('Note', { id: id('Note-2') }).set('title', 'lost')).toThrow(/opening it again failed/);

    writeFormat(paths.volatileBackup, LMDB_FORMAT_VERSION);
    store.reopen();
    engine.create('Note', { id: id('Note-3') }).set('title', 'taken');
    await flushed();
    store.close();

    const reopened = openStore();
    await reopened.hydrate();
    expect(getAttr(id('Note-3'), 'title')).toBe('taken');
    expect(getAttr(id('Note-2'), 'title')).toBeNull();
  });

  it('is recorded in a database this version writes', async () => {
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    await flushed();
    store.close();
    expect(storedFormat(paths.primary)).toBe(LMDB_FORMAT_VERSION);
    expect(storedFormat(paths.volatileBackup)).toBe(LMDB_FORMAT_VERSION);
  });

  it('refuses files that predate it, which are format 1, until they are converted', async () => {
    openStore().close();
    // Files from before the format was recorded, holding data: an attribute row per list index
    writeFormat(paths.primary);
    const root = openEnv({ path: paths.primary, maxDbs: 8, compression: true });
    root.openDB({ name: 'entities', encoding: 'json' }).putSync('Note-1', { type: 'Note', createdAt: 1 });
    root.openDB({ name: 'attrs', encoding: 'json' }).putSync('title\x1FNote-1\x1F0', { t: 'string', v: 'kept' });
    root.close();

    const format1 = new RegExp(`storage format 1, but this version reads format ${LMDB_FORMAT_VERSION}: convert a copy of it with scripts/ears-convert\\.ts`);
    expect(() => openStore({ readOnly: true, log: () => {} })).toThrow(format1);
    expect(() => openStore()).toThrow(format1);
    expect(storedFormat(paths.primary)).toBeUndefined();

    convertToCurrentFormat(paths.primary);
    expect(storedFormat(paths.primary)).toBe(LMDB_FORMAT_VERSION);
    const converted = openStore({ log: () => {} });
    await converted.hydrate();
    expect(getAttr(id('Note-1'), 'title')).toBe('kept');
  });

  it('records the format in new, empty files, which hold nothing to misread', () => {
    // A version that recorded no format, and wrote nothing
    for (const dir of [paths.primary, paths.volatileBackup]) {
      const root = openEnv({ path: dir, maxDbs: 8, compression: true });
      for (const name of ['entities', 'attrs', 'relations']) root.openDB({ name, encoding: 'json' });
      root.close();
    }
    openStore().close();
    expect(storedFormat(paths.primary)).toBe(LMDB_FORMAT_VERSION);
  });

  it('refuses a run history in another format, saying it can be deleted, and leaves the files closed', () => {
    openStore().close();
    writeFormat(paths.volatileBackup, LMDB_FORMAT_VERSION + 1);

    expect(() => openStore()).toThrow(
      new RegExp(`storage format ${LMDB_FORMAT_VERSION + 1}[\\s\\S]*run history only: deleting ${paths.volatileBackup} loses that and nothing else`),
    );

    // Deleting it is enough: the data partition was never the problem
    fs.rmSync(paths.volatileBackup, { recursive: true, force: true });
    const store = openStore();
    expect(store.isOpen()).toBe(true);
  });

  it('refuses a directory with no database in it when read-only', () => {
    fs.mkdirSync(paths.primary, { recursive: true });
    expect(() => openStore({ readOnly: true })).toThrow(`No LMDB database at ${paths.primary}`);
  });

  it('refuses another format, saying which, and leaves the files closed', async () => {
    openStore().close();
    writeFormat(paths.primary, LMDB_FORMAT_VERSION + 1);

    const newer = new RegExp(`storage format ${LMDB_FORMAT_VERSION + 1}, but this version reads format ${LMDB_FORMAT_VERSION}: it was written by a newer apack`);
    expect(() => openStore()).toThrow(newer);
    expect(() => openStore({ readOnly: true })).toThrow(newer);

    writeFormat(paths.primary, 'nonsense');
    expect(() => openStore()).toThrow(new RegExp(`storage format "nonsense", but this version reads format ${LMDB_FORMAT_VERSION}$`));

    // The environment it opened is closed, so the files can be replaced and opened again
    fs.rmSync(paths.primary, { recursive: true, force: true });
    const store = openStore();
    engine.create('Note', { id: id('Note-1') }).set('title', 'after');
    await flushed();
    expect(store.read('primary').getAttr('title', 'Note-1')).toBe('after');
    expect(storedFormat(paths.primary)).toBe(LMDB_FORMAT_VERSION);
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

  it('copies a partition into a directory of its own, readable on its own', async () => {
    const store = openStore({ log: () => {} });
    engine.create('Note', { id: id('Note-1') }).set('title', 'kept');
    await flushed();

    const target = path.join(fs.mkdtempSync(path.join(root, 'snap-')), 'lmdb');
    await store.copyTo('primary', target);

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
    const copying = store.copyTo('primary', target);
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
    await expect(store.copyTo('primary', target)).rejects.toThrow('The LMDB store is closed');
  });
});
