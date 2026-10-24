import { describe, it, expect, beforeAll, afterAll, vi } from 'vitest';
import * as path from 'path';
import * as os from 'os';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { openEnvAt, makeLmdbAdapter, LmdbReader, decodeAttr, type LmdbDbs } from '../../src/lmdb/index.ts';

function tmpDir(label: string) {
  return path.join(os.tmpdir(), `vitest-persistence-${label}-${crypto.randomBytes(4).toString('hex')}`);
}

// ---------------------------------------------------------------------------
// A. LMDB Adapter
// ---------------------------------------------------------------------------
describe('LMDB Adapter', () => {
  let dbs: LmdbDbs;
  let dir: string;

  beforeAll(() => {
    dir = tmpDir('adapter');
    dbs = openEnvAt(dir);
  });

  afterAll(() => {
    try { dbs.entities.close(); dbs.attrs.close(); dbs.relations.close(); dbs.root.close(); } catch {}
    fs.rmSync(dir, { recursive: true, force: true });
  });

  it('close() flushes pending writes synchronously', () => {
    const adapter = makeLmdbAdapter(dbs);
    adapter.onCreateEntity('Entity-flush', 'TestType');
    adapter.onPutAttr('attr1', 'Entity-flush', ['v1', 'v2']);
    adapter.close?.();

    // Data should be readable after close
    const rec = dbs.entities.get('Entity-flush');
    expect(rec).toBeTruthy();
    expect(rec.type).toBe('TestType');
  });

  it("hard-deletes an entity's row and its attributes of every kind, and nothing of another entity", () => {
    const adapter = makeLmdbAdapter(dbs);
    for (const id of ['Task-1', 'Task-10', 'Note-1']) {
      adapter.onCreateEntity(id, id.split('-')[0]);
      adapter.onPutAttr('title', id, [`${id} title`]);
      adapter.onPutAttr('tags', id, ['a', 'b']);
    }
    adapter.onPutAttr('zeta', 'Task-1', 'last kind');
    adapter.close?.();

    const deleting = makeLmdbAdapter(dbs);
    deleting.onDestroyEntity('Task-1');
    deleting.close?.();

    const keysOf = (id: string) => [...dbs.attrs.getKeys()].map(String).filter((key) => key.split('\x1F')[1] === id);
    expect(dbs.entities.get('Task-1')).toBeUndefined();
    expect(keysOf('Task-1')).toEqual([]);
    expect(keysOf('Task-10')).toEqual(['tags\x1FTask-10', 'title\x1FTask-10']);
    expect(keysOf('Note-1')).toHaveLength(2);
    expect(dbs.entities.get('Task-10')).toBeTruthy();
  });

  it("writes an entity's row, holding only its type, when it is created and never for an attribute or a relation", () => {
    const adapter = makeLmdbAdapter(dbs);
    adapter.onCreateEntity('Task-typed', 'Task');
    adapter.onPutAttr('title', 'Task-attr-only', 'no row');
    adapter.onAddRelation('Relation-rows', 'refs', 'Task-src-end', 'Task-tgt-end', undefined);
    adapter.close?.();

    expect(dbs.entities.get('Task-typed')).toEqual({ type: 'Task' });
    for (const id of ['Task-attr-only', 'Task-src-end', 'Task-tgt-end', 'Relation-rows']) expect(dbs.entities.get(id), id).toBeUndefined();
  });

  /**
   * An lmdb range is a live cursor holding a read transaction, and an environment has 126 reader slots.
   * A read that stops early must release its cursor: one that reads the first entry with
   * `it[Symbol.iterator]().next()` never does, so every call that finds a row leaks a slot. The 127th throws `MDB_READERS_FULL`, and from
   * then on every read fails with "No transaction to renew" and every write with EINVAL — for the life of
   * the process. A production app discarded 3,187 writes that way and lost the user's notes.
   *
   * 200 is deliberately past 126: at 126 this passes whatever the implementation does.
   */
  it('does not leak a reader slot per call, however many times it is asked', async () => {
    const adapter = makeLmdbAdapter(dbs);
    adapter.onCreateEntity('Doc-readers', 'Doc');
    adapter.onPutAttr('tags', 'Doc-readers', ['a', 'b', 'c']);
    adapter.close?.();

    const query = new LmdbReader(dbs);
    for (let i = 0; i < 200; i++) {
      expect([...query.entitiesHavingAttr('tags', 1)], `call ${i}`).toEqual(['Doc-readers']);
      // A write transaction between reads, as the app does: it is what makes a leaked slot unrecoverable
      dbs.entities.transactionSync(() => { dbs.entities.put('Doc-readers-probe', { v: i }); });
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
  });


  it('rejects forbidden \\x1F separator in keys', () => {
    const adapter = makeLmdbAdapter(dbs);
    expect(() => adapter.onPutAttr('bad\x1Fkey', 'Entity-1', ['v']))
      .toThrow('forbidden separator');
    adapter.close?.();
  });

  it.each([
    { src: 'undefined', tgt: 'Document-1', label: '"undefined" as src' },
    { src: 'Document-1', tgt: 'undefined', label: '"undefined" as tgt' },
    { src: 'null', tgt: 'Document-1', label: '"null" as src' },
    { src: 'Document-1', tgt: 'null', label: '"null" as tgt' },
    { src: '', tgt: 'Document-1', label: 'empty src' },
    { src: 'Document-1', tgt: '', label: 'empty tgt' },
    { src: 'nohyphen', tgt: 'Document-1', label: 'no-hyphen src' },
    { src: 'Document-1', tgt: 'nohyphen', label: 'no-hyphen tgt' },
  ])('onAddRelation refuses invalid input, and counts the write it drops: $label', ({ src, tgt }) => {
    const onWriteFailure = vi.fn();
    const adapter = makeLmdbAdapter(dbs, onWriteFailure);
    adapter.onAddRelation('Relation-bad', 'TEST', src, tgt, null);
    expect(onWriteFailure).toHaveBeenCalledWith({ op: 'relation', key: 'Relation-bad', error: expect.objectContaining({ message: expect.stringContaining('Invalid') }) });
    // Counted too, so the store's close() reports it
    expect(adapter.getErrorStats?.()).toMatchObject({ errorCount: 1, lastError: { op: 'relation', key: 'Relation-bad' } });
    adapter.close?.();
  });

  it('onUpdateRelation counts an update to a relation that is not there', () => {
    const onWriteFailure = vi.fn();
    const adapter = makeLmdbAdapter(dbs, onWriteFailure);
    adapter.onUpdateRelation('Relation-nonexistent', { info: 'test' });
    expect(onWriteFailure).toHaveBeenCalledWith({ op: 'relation', key: 'Relation-nonexistent', error: expect.objectContaining({ message: expect.stringContaining('missing relId') }) });
    expect(adapter.getErrorStats?.()).toMatchObject({ errorCount: 1, lastError: { op: 'relation', key: 'Relation-nonexistent' } });
    adapter.close?.();
  });

  it.each([
    { patch: { source: 'undefined' }, label: 'source="undefined"' },
    { patch: { target: 'undefined' }, label: 'target="undefined"' },
    { patch: { source: 'null' }, label: 'source="null"' },
    { patch: { target: 'null' }, label: 'target="null"' },
    { patch: { source: 'nohyphen' }, label: 'source without hyphen' },
    { patch: { target: 'nohyphen' }, label: 'target without hyphen' },
  ])('onUpdateRelation refuses invalid source/target values, and counts the write it drops: $label', ({ patch }) => {
    const adapter = makeLmdbAdapter(dbs);
    // First create a valid relation so onUpdateRelation finds it
    adapter.onAddRelation('Relation-valid', 'TEST', 'Doc-1', 'Doc-2', null);
    adapter.close?.(); // flush to DB

    const onWriteFailure = vi.fn();
    const adapter2 = makeLmdbAdapter(dbs, onWriteFailure);
    adapter2.onUpdateRelation('Relation-valid', patch);
    expect(onWriteFailure).toHaveBeenCalledWith({ op: 'relation', key: 'Relation-valid', error: expect.objectContaining({ message: expect.stringContaining('Invalid') }) });
    expect(adapter2.getErrorStats?.()).toMatchObject({ errorCount: 1, lastError: { op: 'relation', key: 'Relation-valid' } });
    adapter2.close?.();
  });
});

describe('LMDB Adapter, a unit', () => {
  /** A real environment where writing `poisoned`'s row fails */
  function guardedEnv(label: string, poisoned: string) {
    const dir = tmpDir(label);
    const live = openEnvAt(dir);
    const guarded = {
      ...live,
      entities: Object.assign(Object.create(Object.getPrototypeOf(live.entities)), live.entities, {
        put: (key: string, value: unknown) => {
          if (key === poisoned) throw new Error('refused');
          return (live.entities as any).put(key, value);
        },
      }),
    } as unknown as LmdbDbs;
    const done = () => {
      try { live.entities.close(); live.attrs.close(); live.relations.close(); live.root.close(); } catch {}
      fs.rmSync(dir, { recursive: true, force: true });
    };
    return { live, guarded, done };
  }

  it('is stored whole or not at all: one row refusing loses the unit, and reports each of its rows', () => {
    const { live, guarded, done } = guardedEnv('unit-whole', 'Document-poison');
    const dropped: Array<{ op: string; key: string }> = [];
    const adapter = makeLmdbAdapter(guarded, ({ op, key }) => dropped.push({ op, key }));

    adapter.onCreateEntity('Document-before', 'Document');
    adapter.onUnitStart();
    adapter.onCreateEntity('Document-ok', 'Document');
    adapter.onCreateEntity('Document-poison', 'Document');
    adapter.onUnitEnd();

    expect(live.entities.get('Document-before'), 'written before the unit, on its own').toBeTruthy();
    expect(live.entities.get('Document-ok'), 'part of the unit that failed').toBeUndefined();
    expect(dropped).toEqual([{ op: 'entity', key: 'Document-ok' }, { op: 'entity', key: 'Document-poison' }]);
    adapter.close?.();
    done();
  });

  it('deletes an entity at the end of the unit, not when the destroy is made', () => {
    const { live, guarded, done } = guardedEnv('unit-destroy', 'none');
    const adapter = makeLmdbAdapter(guarded);
    adapter.onCreateEntity('Document-1', 'Document');
    adapter.onPutAttr('title', 'Document-1', 'kept');
    adapter.close?.();
    const reopened = makeLmdbAdapter(guarded);

    reopened.onUnitStart();
    reopened.onDestroyEntity('Document-1');
    expect(live.entities.get('Document-1'), 'still there inside the unit').toBeTruthy();
    reopened.onUnitEnd();
    expect(live.entities.get('Document-1')).toBeUndefined();
    expect(live.attrs.get(`title\x1FDocument-1`)).toBeUndefined();
    reopened.close?.();
    done();
  });
});

// ---------------------------------------------------------------------------
// B. LMDB Adapter error stats
// ---------------------------------------------------------------------------
describe('LMDB Adapter error stats', () => {
  /** An environment whose writes all fail, as a full disk would */
  const failing = (): LmdbDbs => {
    const fail = () => { throw new Error('disk full'); };
    const db = { transactionSync: fail, put: fail, remove: fail, get: () => undefined, doesExist: () => false, getKeys: () => [], getRange: () => [] };
    return { entities: db, attrs: db, relations: db, root: db } as unknown as LmdbDbs;
  };

  it('counts a delete it could not write, so a caller is never told the row is gone', () => {
    const adapter = makeLmdbAdapter(failing());
    adapter.onDestroyEntity('Document-1');
    expect(adapter.getErrorStats?.()).toMatchObject({ errorCount: 1, lastError: { op: 'destroy', key: 'Document-1' } });
  });

  // The count is of rows that did not reach disk, not of batches that failed: one array write is an entity
  // row and an attribute row, and a caller that is told "1" when two rows are gone has been misinformed.
  it('counts every row it could not write when the sink closes, and names the last', () => {
    const adapter = makeLmdbAdapter(failing());
    adapter.onCreateEntity('Document-1', 'Document');
    adapter.onPutAttr('title', 'Document-1', 'kept');
    adapter.close?.();
    expect(adapter.getErrorStats?.()).toMatchObject({
      errorCount: 2,
      lastError: { op: 'final flush', key: `title\x1FDocument-1` },
    });
  });

  // The failure this exists to prevent: one bad row used to abort the whole transaction and then clear every
  // buffer, so unrelated writes in the same microtask were lost with it. That is how a note died as
  // collateral damage for something else's failure.
  it('keeps the writes that work when one row cannot be written, and names the one that could not', async () => {
    const dir = tmpDir('isolation');
    const live = openEnvAt(dir);
    const dropped: Array<{ op: string; key: string }> = [];
    // Only this one row refuses; everything else goes through to the real store
    const poisoned = 'Document-poison';
    const guarded = {
      ...live,
      entities: Object.assign(Object.create(Object.getPrototypeOf(live.entities)), live.entities, {
        put: (key: string, value: unknown) => {
          if (key === poisoned) throw new Error('refused');
          return (live.entities as any).put(key, value);
        },
      }),
    } as unknown as LmdbDbs;

    const adapter = makeLmdbAdapter(guarded, ({ op, key }) => dropped.push({ op, key }));
    adapter.onCreateEntity('Document-ok-1', 'Document');
    adapter.onCreateEntity(poisoned, 'Document');
    adapter.onCreateEntity('Document-ok-2', 'Document');
    adapter.close?.();

    expect(dropped, 'only the row that refused').toEqual([{ op: 'entity', key: poisoned }]);
    expect(live.entities.get('Document-ok-1'), 'written before the bad one').toBeTruthy();
    expect(live.entities.get('Document-ok-2'), 'written after the bad one').toBeTruthy();
    expect(adapter.getErrorStats?.().errorCount).toBe(1);

    try { live.entities.close(); live.attrs.close(); live.relations.close(); live.root.close(); } catch {}
    fs.rmSync(dir, { recursive: true, force: true });
  });
});

// ---------------------------------------------------------------------------
// C. Query Layer
// ---------------------------------------------------------------------------
describe('Query Layer', () => {
  let dbs: LmdbDbs;
  let dir: string;
  let query: LmdbReader;

  beforeAll(() => {
    dir = tmpDir('query');
    dbs = openEnvAt(dir);
    const adapter = makeLmdbAdapter(dbs);

    adapter.onPutAttr('TestKind', 'Entity-123', ['valid1', 'valid2', 'valid3']);

    // Date attr
    const now = new Date('2025-06-01T00:00:00.000Z');
    adapter.onCreateEntity('Entity-456', 'TestEntity');
    adapter.onPutAttr('CreatedAt', 'Entity-456', now);

    // Complex object attr
    adapter.onCreateEntity('Entity-789', 'TestEntity');
    adapter.onPutAttr('Config', 'Entity-789', { nested: { value: 42 }, arr: [1, 2, 3] });

    // Entities + relations for neighbor tests
    adapter.onCreateEntity('Doc-1', 'Document');
    adapter.onCreateEntity('Doc-2', 'Document');
    adapter.onCreateEntity('Doc-3', 'Document');
    adapter.onAddRelation('Rel-1', 'LINKS', 'Doc-1', 'Doc-2', null);
    adapter.onAddRelation('Rel-2', 'LINKS', 'Doc-1', 'Doc-3', null);

    // Pagination entities
    for (let i = 0; i < 10; i++) {
      adapter.onCreateEntity(`Page-${i}`, 'Page');
      adapter.onPutAttr('Index', `Page-${i}`, i);
    }

    adapter.close?.(); // flush
    query = new LmdbReader(dbs);
  });

  afterAll(() => {
    try { dbs.entities.close(); dbs.attrs.close(); dbs.relations.close(); dbs.root.close(); } catch {}
    fs.rmSync(dir, { recursive: true, force: true });
  });

  it('getAttr reads an attribute\'s one value, a list whole', () => {
    expect(query.getAttr('Entity-123', 'TestKind')).toEqual(['valid1', 'valid2', 'valid3']);
    expect(query.getAttr('Page-4', 'Index')).toBe(4);
    expect(query.getAttr('Page-none', 'Index')).toBeUndefined();
  });

  it('findEntitiesByAttr matches a value or an element of a list, and a predicate sees the whole value', () => {
    expect(query.findEntitiesByAttr('TestKind', { equals: 'valid2' })).toEqual(['Entity-123']);
    expect(query.findEntitiesByAttr('Index', { predicate: (value) => (value as number) > 7 })).toEqual(['Page-8', 'Page-9']);
  });

  it('findEntitiesByAttr Date equality (instance + timestamp)', () => {
    const target = new Date('2025-06-01T00:00:00.000Z');
    const byDate = query.findEntitiesByAttr('CreatedAt', {
      equals: target,
      entityType: 'TestEntity',
    });
    expect(byDate).toContain('Entity-456');

    const byTimestamp = query.findEntitiesByAttr('CreatedAt', {
      equals: target.getTime(),
      entityType: 'TestEntity',
    });
    expect(byTimestamp).toContain('Entity-456');
  });

  it('findEntitiesByAttr deep equality for complex objects', () => {
    const found = query.findEntitiesByAttr('Config', {
      equals: { nested: { value: 42 }, arr: [1, 2, 3] },
      deepEquals: true,
    });
    expect(found).toContain('Entity-789');
  });

  it('neighbors() follows relations of a kind, out and in', () => {
    expect(query.neighbors('Doc-1', { kind: 'LINKS' }).sort()).toEqual(['Doc-2', 'Doc-3']);
    expect(query.neighbors('Doc-2', { kind: 'LINKS', direction: 'in' })).toEqual(['Doc-1']);
  });

  it('entitiesHavingAttr() and relations() respect limit', () => {
    const limited = [...query.entitiesHavingAttr('Index', 3)];
    expect(limited.length).toBe(3);

    let relCount = 0;
    for (const _ of query.relations({ limit: 1 })) relCount++;
    expect(relCount).toBeLessThanOrEqual(1);
  });

  it('decodeAttr() is exported and decodes dates', () => {
    const dateRec = { t: 'date' as const, v: '2025-01-01T00:00:00.000Z' };
    const decoded = decodeAttr(dateRec);
    expect(decoded).toBeInstanceOf(Date);
    expect((decoded as Date).toISOString()).toBe('2025-01-01T00:00:00.000Z');
  });
});
