// @slow: one of its 48 cases is 1.7s of the file's 2.8s — 200 LMDB round-trips past the 126 reader-slot limit
// `does not leak a reader slot per call` is deliberately past that limit, for the reason its own comment
// gives: at 126 it passes whatever the implementation does, and the leak it guards cost a production app
// 3,187 discarded writes. The 200 write transactions are where the time is, not the loop's turns —
// measured 2026-10-06, 200 `setTimeout(resolve, 0)` turns are 235ms and 200 `setImmediate` turns are 3ms,
// so the clamp is 13% of that case and swapping it leaves the file slow. The other 47 cases are ~1s
// together, so this is one case's cost rather than the file's shape.
import { describe, it, expect, beforeAll, afterAll, vi } from 'vitest';
import * as path from 'path';
import * as os from 'os';
import * as fs from 'fs';
import * as crypto from 'crypto';
import type { EARS } from '../../src/entities.ts';
import { openEnvAt, makeLmdbAdapter, LmdbQuery, decodeAttr, type LmdbDbs } from '../../src/lmdb/index.ts';
import { makePolicy, makeShardedPersistence, type Partition, type PersistenceSink } from '../../src/index.ts';

const Entity = {
  Document: 'Document' as EARS.Entity,
  TNode: 'TNode' as EARS.Entity,
};

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
    adapter.onPutAttrArray('attr1', 'Entity-flush', ['v1', 'v2']);
    adapter.close?.();

    // Data should be readable after close
    const rec = dbs.entities.get('Entity-flush');
    expect(rec).toBeTruthy();
    expect(rec.type).toBe('TestType');
  });

  it("hard-deletes an entity's row and its attributes of every kind, and nothing of another entity", () => {
    const adapter = makeLmdbAdapter(dbs);
    for (const id of ['Task-1', 'Task-10', 'Note-1']) {
      adapter.onCreateEntity(id);
      adapter.onPutAttrArray('title', id, [`${id} title`]);
      adapter.onPutAttrArray('tags', id, ['a', 'b']);
    }
    adapter.onPutAttrArray('zeta', 'Task-1', ['last kind']);
    adapter.close?.();

    const deleting = makeLmdbAdapter(dbs);
    deleting.onDestroyEntity('Task-1');
    deleting.close?.();

    const keysOf = (id: string) => [...dbs.attrs.getKeys()].map(String).filter((key) => key.split('\x1F')[1] === id);
    expect(dbs.entities.get('Task-1')).toBeUndefined();
    expect(keysOf('Task-1')).toEqual([]);
    expect(keysOf('Task-10')).toHaveLength(3);
    expect(keysOf('Note-1')).toHaveLength(3);
    expect(dbs.entities.get('Task-10')).toBeTruthy();
  });

  it("types an entity's row by its id's prefix, unless it's created with another type", () => {
    const adapter = makeLmdbAdapter(dbs);
    adapter.onCreateEntity('Task-typed', 'Task');
    adapter.onCreateEntity('Task-special', 'Special');
    adapter.close?.();

    expect(dbs.entities.get('Task-typed').type).toBe('Task');
    expect(dbs.entities.get('Task-special').type).toBe('Special');
  });

  /**
   * An lmdb range is a live cursor holding a read transaction, and an environment has 126 reader slots.
   * `getAttrLength` used to read the first entry with `it[Symbol.iterator]().next()`, which never releases
   * the cursor, so every call that found a row leaked a slot. The 127th throws `MDB_READERS_FULL`, and from
   * then on every read fails with "No transaction to renew" and every write with EINVAL — for the life of
   * the process. A production app discarded 3,187 writes that way and lost the user's notes.
   *
   * 200 is deliberately past 126: at 126 this passes whatever the implementation does.
   */
  it('does not leak a reader slot per call, however many times it is asked', async () => {
    const adapter = makeLmdbAdapter(dbs);
    adapter.onCreateEntity('Doc-readers');
    adapter.onPutAttrArray('tags', 'Doc-readers', ['a', 'b', 'c']);
    adapter.close?.();

    const query = new LmdbQuery(dbs);
    for (let i = 0; i < 200; i++) {
      expect(query.getAttrLength('tags', 'Doc-readers'), `call ${i}`).toBe(3);
      // A write transaction between reads, as the app does: it is what makes a leaked slot unrecoverable
      dbs.entities.transactionSync(() => { dbs.entities.put('Doc-readers-probe', { v: i }); });
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
  });


  it('rejects forbidden \\x1F separator in keys', () => {
    const adapter = makeLmdbAdapter(dbs);
    expect(() => adapter.onPutAttrArray('bad\x1Fkey', 'Entity-1', ['v']))
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
    const adapter = makeLmdbAdapter(dbs);
    const spy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    adapter.onAddRelation('Relation-bad', 'TEST', src, tgt, null);
    expect(spy).toHaveBeenCalledWith(expect.stringContaining('[LMDB] Invalid'));
    // Counted, so the store's close() reports it rather than the write vanishing with a console line
    expect(adapter.getErrorStats?.()).toMatchObject({ errorCount: 1, lastError: { op: 'relation', key: 'Relation-bad' } });
    spy.mockRestore();
    adapter.close?.();
  });

  it('onUpdateRelation counts an update to a relation that is not there', () => {
    const adapter = makeLmdbAdapter(dbs);
    const spy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    adapter.onUpdateRelation('Relation-nonexistent', { info: 'test' });
    expect(spy).toHaveBeenCalledWith(expect.stringContaining('missing relId'));
    expect(adapter.getErrorStats?.()).toMatchObject({ errorCount: 1, lastError: { op: 'relation', key: 'Relation-nonexistent' } });
    spy.mockRestore();
    adapter.close?.();
  });

  it.each([
    { patch: { src: 'undefined' }, label: 'src="undefined"' },
    { patch: { tgt: 'undefined' }, label: 'tgt="undefined"' },
    { patch: { src: 'null' }, label: 'src="null"' },
    { patch: { tgt: 'null' }, label: 'tgt="null"' },
    { patch: { src: 'nohyphen' }, label: 'src without hyphen' },
    { patch: { tgt: 'nohyphen' }, label: 'tgt without hyphen' },
  ])('onUpdateRelation refuses invalid src/tgt values, and counts the write it drops: $label', ({ patch }) => {
    const adapter = makeLmdbAdapter(dbs);
    // First create a valid relation so onUpdateRelation finds it
    adapter.onAddRelation('Relation-valid', 'TEST', 'Doc-1', 'Doc-2', null);
    adapter.close?.(); // flush to DB

    const adapter2 = makeLmdbAdapter(dbs);
    const spy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    adapter2.onUpdateRelation('Relation-valid', patch);
    expect(spy).toHaveBeenCalledWith(expect.stringContaining('[LMDB] Invalid'));
    expect(adapter2.getErrorStats?.()).toMatchObject({ errorCount: 1, lastError: { op: 'relation', key: 'Relation-valid' } });
    spy.mockRestore();
    adapter2.close?.();
  });
});

// ---------------------------------------------------------------------------
// B. Sharded Router
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
    adapter.onPutAttrArray('title', 'Document-1', ['kept']);
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
    adapter.onCreateEntity('Document-ok-1');
    adapter.onCreateEntity(poisoned);
    adapter.onCreateEntity('Document-ok-2');
    adapter.close?.();

    expect(dropped, 'only the row that refused').toEqual([{ op: 'entity', key: poisoned }]);
    expect(live.entities.get('Document-ok-1'), 'written before the bad one').toBeTruthy();
    expect(live.entities.get('Document-ok-2'), 'written after the bad one').toBeTruthy();
    expect(adapter.getErrorStats?.().errorCount).toBe(1);

    try { live.entities.close(); live.attrs.close(); live.relations.close(); live.root.close(); } catch {}
    fs.rmSync(dir, { recursive: true, force: true });
  });
});

describe('Sharded Router', () => {
  let dirs: { primary: string; volatileBackup: string };
  let envs: Record<Partition, LmdbDbs>;
  let sinks: Record<Partition, PersistenceSink>;
  let policy: ReturnType<typeof makePolicy>;
  let sharded: ReturnType<typeof makeShardedPersistence>;

  beforeAll(() => {
    dirs = {
      primary: tmpDir('shard-primary'),
      volatileBackup: tmpDir('shard-volatile'),
    };
    envs = {
      primary: openEnvAt(dirs.primary),
      volatileBackup: openEnvAt(dirs.volatileBackup),
    };
    policy = makePolicy({
      excludedEntityTypes: new Set([Entity.TNode]),
    });
    sinks = {
      primary: makeLmdbAdapter(envs.primary),
      volatileBackup: makeLmdbAdapter(envs.volatileBackup),
    };
    sharded = makeShardedPersistence(policy, sinks);
  });

  afterAll(() => {
    sharded.close?.();
    for (const db of Object.values(envs)) {
      try { db.entities.close(); db.attrs.close(); db.relations.close(); db.root.close(); } catch {}
    }
    for (const d of Object.values(dirs)) {
      fs.rmSync(d, { recursive: true, force: true });
    }
  });

  it('onDropAttr throws without entireArray', () => {
    expect(() => (sharded as any).onDropAttr('kind', 'Document-1', 0))
      .toThrow('onDropAttr requires entireArray');
  });

  it('onUpdateRelation throws on null/empty src or tgt in patch', () => {
    sharded.onCreateEntity('Document-a', 'Document');
    sharded.onCreateEntity('Document-b', 'Document');
    sharded.onAddRelation('Relation-1', 'TEST', 'Document-a', 'Document-b', null);

    expect(() => sharded.onUpdateRelation('Relation-1', { src: null as any }))
      .toThrow('patch.src is empty');
    expect(() => sharded.onUpdateRelation('Relation-1', { tgt: '' }))
      .toThrow('patch.tgt is empty');
  });

  it.each([
    { src: '', tgt: 'Document-1', label: 'empty src' },
    { src: null as any, tgt: 'Document-1', label: 'null src' },
    { src: 'Document-1', tgt: '', label: 'empty tgt' },
    { src: 'Document-1', tgt: undefined as any, label: 'undefined tgt' },
    { src: 123 as any, tgt: 'Document-1', label: 'non-string src' },
  ])('hydrateRelationMetadata warns on invalid input: $label', ({ src, tgt }) => {
    const spy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    sharded.hydrateRelationMetadata('Relation-1', 'TEST', src, tgt);
    expect(spy).toHaveBeenCalledWith(expect.stringContaining('[Sharded] Invalid'));
    spy.mockRestore();
  });

  it('getRelMeta() returns independent Map copies', () => {
    const m1 = sharded.getRelMeta();
    const m2 = sharded.getRelMeta();
    expect(m1).not.toBe(m2);
    m1.set('fake', { kind: 'x', src: 'x', tgt: 'x' });
    expect(sharded.getRelMeta().has('fake')).toBe(false);
  });

  it('getErrorStats() aggregates from all sinks', () => {
    const stats = sharded.getErrorStats?.();
    expect(stats).toBeDefined();
    expect(stats).toHaveProperty('errorCount');
    expect(typeof stats!.errorCount).toBe('number');
  });

  it('best-effort update broadcasts to all sinks for unknown relId', () => {
    const spy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    // Should not throw
    expect(() =>
      sharded.onUpdateRelation('Relation-unknown', { info: { x: 1 } })
    ).not.toThrow();
    expect(spy).toHaveBeenCalledWith(
      expect.stringContaining('unknown relId'),
      expect.anything()
    );
    spy.mockRestore();
  });

  it('best-effort removal broadcasts to all sinks for unknown relId', () => {
    // Should not throw even for completely unknown relation
    expect(() => sharded.onRemoveRelation('Relation-mystery')).not.toThrow();
  });

  it("drops a relation's cache entry when the engine removes it, before destroying its ends", () => {
    sharded.onCreateEntity('Document-d1', 'Document');
    sharded.onCreateEntity('Document-d2', 'Document');
    sharded.onAddRelation('Relation-cd1', 'REF', 'Document-d1', 'Document-d2', null);
    expect(sharded.getRelMeta().has('Relation-cd1')).toBe(true);

    // The engine's order (engine/persistence.spec.ts)
    sharded.onRemoveRelation('Relation-cd1');
    sharded.onDestroyEntity('Document-d1');

    expect(sharded.getRelMeta().has('Relation-cd1')).toBe(false);
  });

  it('system continues working after encountering invalid data', () => {
    const spy = vi.spyOn(console, 'warn').mockImplementation(() => {});
    // Feed bad data
    sharded.hydrateRelationMetadata('Relation-bad1', 'TEST', '', 'Document-1');
    spy.mockRestore();

    // Valid operations should still work
    sharded.onCreateEntity('Document-ok', 'Document');
    sharded.onCreateEntity('Document-ok2', 'Document');
    expect(() =>
      sharded.onAddRelation('Relation-ok', 'TEST', 'Document-ok', 'Document-ok2', null)
    ).not.toThrow();
  });
});

// ---------------------------------------------------------------------------
// C. Relation Updates
// ---------------------------------------------------------------------------
describe('Relation Updates', () => {
  let dirs: { primary: string; volatileBackup: string };
  let envs: Record<Partition, LmdbDbs>;
  let sinks: Record<Partition, PersistenceSink>;
  let policy: ReturnType<typeof makePolicy>;
  let sharded: ReturnType<typeof makeShardedPersistence>;

  beforeAll(() => {
    dirs = {
      primary: tmpDir('relupd-primary'),
      volatileBackup: tmpDir('relupd-volatile'),
    };
    envs = {
      primary: openEnvAt(dirs.primary),
      volatileBackup: openEnvAt(dirs.volatileBackup),
    };
    policy = makePolicy({
      excludedEntityTypes: new Set([Entity.TNode]),
    });
    sinks = {
      primary: makeLmdbAdapter(envs.primary),
      volatileBackup: makeLmdbAdapter(envs.volatileBackup),
    };
    sharded = makeShardedPersistence(policy, sinks);

    // Set up entities
    sharded.onCreateEntity('Document-r1', 'Document');
    sharded.onCreateEntity('Document-r2', 'Document');
    sharded.onCreateEntity('TNode-t1', 'TNode');
  });

  afterAll(() => {
    sharded.close?.();
    for (const db of Object.values(envs)) {
      try { db.entities.close(); db.attrs.close(); db.relations.close(); db.root.close(); } catch {}
    }
    for (const d of Object.values(dirs)) {
      fs.rmSync(d, { recursive: true, force: true });
    }
  });

  it('partial update (info only) preserves src/tgt metadata', () => {
    sharded.onAddRelation('Relation-pu1', 'LINKS', 'Document-r1', 'Document-r2', { weight: 0.5 });
    const before = sharded.getRelMeta().get('Relation-pu1');

    sharded.onUpdateRelation('Relation-pu1', { info: { weight: 0.8 } });

    const after = sharded.getRelMeta().get('Relation-pu1');
    expect(after?.src).toBe(before?.src);
    expect(after?.tgt).toBe(before?.tgt);
  });

  it('cross-partition endpoint change moves relation between sinks', () => {
    // Doc -> TNode goes to volatileBackup
    sharded.onAddRelation('Relation-cp1', 'LINKS', 'Document-r1', 'TNode-t1', { w: 1 });
    const partBefore = policy.routeRelation({
      srcType: Entity.Document,
      tgtType: Entity.TNode,
    });
    expect(partBefore).toBe('volatileBackup');

    // Change tgt to Document -> moves to primary
    sharded.onUpdateRelation('Relation-cp1', { tgt: 'Document-r2' });
    const partAfter = policy.routeRelation({
      srcType: Entity.Document,
      tgtType: Entity.Document,
    });
    expect(partAfter).toBe('primary');

    const meta = sharded.getRelMeta().get('Relation-cp1');
    expect(meta?.tgt).toBe('Document-r2');
  });

  it('info preserved during cross-partition move', () => {
    sharded.onAddRelation('Relation-cp2', 'LINKS', 'Document-r1', 'TNode-t1', { important: true });
    // Move to primary by changing tgt
    sharded.onUpdateRelation('Relation-cp2', { tgt: 'Document-r2' });
    // Metadata should still be valid
    const meta = sharded.getRelMeta().get('Relation-cp2');
    expect(meta).toBeDefined();
    expect(meta?.src).toBe('Document-r1');
    expect(meta?.tgt).toBe('Document-r2');
  });
});

// ---------------------------------------------------------------------------
// D. Query Layer
// ---------------------------------------------------------------------------
describe('Query Layer', () => {
  let dbs: LmdbDbs;
  let dir: string;
  let query: LmdbQuery;

  beforeAll(() => {
    dir = tmpDir('query');
    dbs = openEnvAt(dir);
    const adapter = makeLmdbAdapter(dbs);

    // Given
    // Corrupt index key — use transactionSync to avoid unhandled async rejections
    dbs.entities.transactionSync(() => {
      dbs.attrs.put(`TestKind\x1FEntity-123\x1FnotANumber`, { t: 'string', v: 'corrupt' });
    });
    adapter.onPutAttrArray('TestKind', 'Entity-123', ['valid1', 'valid2', 'valid3']);

    // Date attr
    const now = new Date('2025-06-01T00:00:00.000Z');
    adapter.onCreateEntity('Entity-456', 'TestEntity');
    adapter.onPutAttrArray('CreatedAt', 'Entity-456', [now]);

    // Complex object attr
    adapter.onCreateEntity('Entity-789', 'TestEntity');
    adapter.onPutAttrArray('Config', 'Entity-789', [{ nested: { value: 42 }, arr: [1, 2, 3] }]);

    // Entities + relations for neighbor tests
    adapter.onCreateEntity('Doc-1', 'Document');
    adapter.onCreateEntity('Doc-2', 'Document');
    adapter.onCreateEntity('Doc-3', 'Document');
    adapter.onAddRelation('Rel-1', 'LINKS', 'Doc-1', 'Doc-2', null);
    adapter.onAddRelation('Rel-2', 'LINKS', 'Doc-1', 'Doc-3', null);

    // Pagination entities
    for (let i = 0; i < 10; i++) {
      adapter.onCreateEntity(`Page-${i}`, 'Page');
      adapter.onPutAttrArray('Index', `Page-${i}`, [i]);
    }

    // Helper test
    adapter.onCreateEntity('Helper-1', 'Helper');
    adapter.onPutAttrArray('Values', 'Helper-1', [1, 2, 3, 4, 5]);

    adapter.close?.(); // flush
    query = new LmdbQuery(dbs);
  });

  afterAll(() => {
    try { dbs.entities.close(); dbs.attrs.close(); dbs.relations.close(); dbs.root.close(); } catch {}
    fs.rmSync(dir, { recursive: true, force: true });
  });

  it('getAttrArray skips corrupt non-numeric index keys', () => {
    const arr = query.getAttrArray('TestKind', 'Entity-123');
    expect(arr).toEqual(['valid1', 'valid2', 'valid3']);
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

  it('getAttrCount() and getFirstAttr() helpers', () => {
    const count = query.getAttrCount('Values', 'Helper-1');
    expect(count).toBe(5);

    const first = query.getFirstAttr('Values', 'Helper-1');
    expect(first).toBe(1);
  });

  it('decodeAttr() is exported and decodes dates', () => {
    const dateRec = { t: 'date' as const, v: '2025-01-01T00:00:00.000Z' };
    const decoded = decodeAttr(dateRec);
    expect(decoded).toBeInstanceOf(Date);
    expect((decoded as Date).toISOString()).toBe('2025-01-01T00:00:00.000Z');
  });
});

// ---------------------------------------------------------------------------
// E. Partition Routing (pure unit tests on policy)
// ---------------------------------------------------------------------------
describe('Partition Routing', () => {
  const policy = makePolicy({
    excludedEntityTypes: new Set([Entity.TNode]),
  });

  it('routes Document -> primary and TNode -> volatileBackup', () => {
    expect(policy.routeEntity('Document-1')).toBe('primary');
    expect(policy.routeEntity('TNode-1')).toBe('volatileBackup');
  });

  it('routes relations based on endpoint types', () => {
    // Both primary -> primary
    expect(policy.routeRelation({
      srcType: Entity.Document,
      tgtType: Entity.Document,
    })).toBe('primary');

    // Either excluded -> volatile
    expect(policy.routeRelation({
      srcType: Entity.Document,
      tgtType: Entity.TNode,
    })).toBe('volatileBackup');

    expect(policy.routeRelation({
      srcType: Entity.TNode,
      tgtType: Entity.Document,
    })).toBe('volatileBackup');

  });
});
