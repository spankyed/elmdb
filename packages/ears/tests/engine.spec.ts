import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import {
  BatchSlot,
  EarsError,
  createEarsEngine,
  encodeBatch,
  isEarsError,
  type Schema,
} from '../src/index.ts';
import {
  DATABASE_FORMAT,
  createTemporaryDatabases,
  loadDump,
  openDatabase,
  openDatabaseAsync,
  openEngine,
  openEngineAsync,
} from '../src/lmdb/index.ts';

const baseSchema: Schema = {
  types: {
    User: {
      fields: {
        name: { type: 'string' },
        email: { type: 'string', optional: true },
        role: { type: 'string', optional: true },
      },
      indexes: {
        byEmail: { kind: 'value', fields: ['email'], unique: true },
      },
    },
    Task: {
      fields: {
        title: { type: 'string' },
        done: { type: 'boolean' },
        priority: { type: 'integer', optional: true },
        assignee: { type: 'ref', to: ['User'], onDelete: 'setNull', optional: true, nullable: true },
      },
      indexes: {
        byDone: { kind: 'value', fields: ['done'] },
      },
    },
  },
  relations: {
    blocks: {
      cardinality: 'manyToMany',
      endpoints: [{ source: 'Task', target: 'Task' }],
      acyclic: true,
      weighted: true,
    },
  },
};

const temps = createTemporaryDatabases();
const extraDirs: string[] = [];

afterEach(() => {
  temps.disposeAll();
  for (const dir of extraDirs.splice(0)) {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

describe('@elmdb/ears engine, batches, hooks, schema evolution & durability', () => {
  it('supports CRUD, intra-batch BatchSlot refs, edges, and rejects async batch callbacks', () => {
    expect(DATABASE_FORMAT).toBe('elmdb/2');
    const handle = temps.open({ schema: baseSchema });
    const engine = createEarsEngine({ database: handle.database });

    expect(engine.admin.readOnly).toBe(false);
    expect(engine.admin.openReport().status).toBe('fresh');

    const res = engine.batch((b) => {
      const u = b.create('User', { name: 'Ada', email: 'ada@example.com' }, 'u-ada');
      expect(u).toBeInstanceOf(BatchSlot);
      const t1 = b.create('Task', { title: 'Design v2', done: false, priority: 1, assignee: 'u-ada' }, 't-1');
      const t2 = b.create('Task', { title: 'Ship v2', done: false, priority: 2, assignee: 'u-ada' }, 't-2');
      b.link(t1, 'blocks', t2, { weight: 3.5, info: { reason: 'milestone' } });
      expect(b.length).toBe(4);
    });
    expect(res.createdIds).toEqual(['u-ada', 't-1', 't-2']);

    const t1Row = engine.get('t-1');
    expect(t1Row).toEqual({
      id: 't-1',
      typeName: 'Task',
      trashed: false,
      fields: { title: 'Design v2', done: false, priority: 1, assignee: 'u-ada' },
    });

    const outEdges = engine.getEdges('t-1', { relKind: 'blocks', dir: 'out' });
    expect(outEdges).toHaveLength(1);
    expect(outEdges[0]?.targetId).toBe('t-2');
    expect(outEdges[0]?.weight).toBe(3.5);

    engine.patch('t-1', { done: true });
    expect(engine.get('t-1')?.fields['done']).toBe(true);

    engine.unset('t-1', ['priority']);
    expect(engine.get('t-1')?.fields['priority']).toBeUndefined();

    engine.trash('t-2');
    expect(engine.get('t-2')?.trashed).toBe(true);
    engine.restore('t-2');
    expect(engine.get('t-2')?.trashed).toBe(false);

    // Referential integrity onDelete: setNull clears assignee when u-ada is destroyed!
    engine.destroy('u-ada');
    expect(engine.get('u-ada')).toBeNull();
    expect(engine.get('t-1')?.fields['assignee']).toBeNull();

    // Unlink works cleanly
    engine.unlink('t-1', 'blocks', 't-2');
    expect(engine.getEdges('t-1', { dir: 'out' })).toEqual([]);

    // Async batch callbacks fail synchronously with ASYNC_CALLBACK_FORBIDDEN
    let asyncErr: unknown;
    try {
      encodeBatch((async () => {}) as unknown as () => void);
    } catch (err) {
      asyncErr = err;
    }
    expect(isEarsError(asyncErr)).toBe(true);
    expect((asyncErr as EarsError).code).toBe('ASYNC_CALLBACK_FORBIDDEN');
  });

  it('enforces synchronous beforeCommit hooks inside native savepoints (abort + derived cascade)', () => {
    const handle = temps.open({
      schema: {
        types: {
          Note: { fields: { title: { type: 'string' }, slug: { type: 'string', optional: true } } },
          Audit: { fields: { target: { type: 'string' }, kind: { type: 'string' } } },
        },
        relations: {},
      },
    });
    const engine = createEarsEngine({ database: handle.database });

    const seenOps: string[] = [];
    const unsub = engine.beforeCommit(['Note'], (delta) => {
      for (const ev of delta.entities) {
        seenOps.push(`${ev.op}:${ev.id}`);
        if (ev.id === 'n-bad') {
          throw new Error('forbidden note id');
        }
      }
      if (delta.entities.some((e) => e.op === 'create')) {
        return (b) => {
          for (const ev of delta.entities) {
            if (ev.op === 'create') {
              b.create('Audit', { target: ev.id, kind: 'created' }, `aud-${ev.id}`);
            }
          }
        };
      }
      return undefined;
    });

    engine.create('Note', { title: 'Hello' }, 'n-1');
    expect(engine.get('aud-n-1')?.fields).toEqual({ target: 'n-1', kind: 'created' });
    expect(seenOps).toEqual(['create:n-1']);

    // Throwing in beforeCommit rolls back the entire batch atomically!
    let abortErr: unknown;
    try {
      engine.batch((b) => {
        b.create('Note', { title: 'Good neighbor' }, 'n-neighbor');
        b.create('Note', { title: 'Bad' }, 'n-bad');
      });
    } catch (err) {
      abortErr = err;
    }
    expect(isEarsError(abortErr)).toBe(true);
    expect((abortErr as EarsError).code).toBe('HOOK_ABORTED');
    expect(engine.get('n-neighbor')).toBeNull();
    expect(engine.get('n-bad')).toBeNull();

    // Unsubscribing is idempotent and stops invoking the hook.
    unsub();
    unsub();
    engine.create('Note', { title: 'Allowed now' }, 'n-bad');
    expect(engine.get('n-bad')).not.toBeNull();
  });

  it('handles O(1) schema renames, archive/purge, backup, dump/load, and async openers', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-v2-admin-'));
    const bakDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-v2-bak-'));
    const dumpFile = path.join(os.tmpdir(), `ears-v2-dump-${Date.now()}-${Math.random().toString(16).slice(2)}.json`);
    const loadedDir = path.join(os.tmpdir(), `ears-v2-loaded-${Date.now()}-${Math.random().toString(16).slice(2)}`);
    extraDirs.push(dir, bakDir, loadedDir);

    try {
      const engine = await openEngineAsync({
        path: dir,
        schema: baseSchema,
        durability: 'strict',
        buildPolicy: 'eager',
      });
      engine.create('Task', { title: 'Rename me', done: false }, 't-1');
      expect(engine.admin.renameField('Task', 'title', 'heading')).toBe(2);
      expect(engine.get('t-1')?.fields['heading']).toBe('Rename me');

      const gotSchema = engine.admin.getSchema();
      expect(gotSchema.types?.['Task']?.fields?.['heading']).toBeDefined();

      expect((await engine.admin.checkAsync()).clean).toBe(true);
      await engine.admin.flushAsync();
      await engine.admin.rebuildAsync();
      expect(engine.admin.stats().typeCounts['Task']).toBe(1);
      expect(engine.admin.indexStatus().length).toBeGreaterThan(0);

      await engine.admin.backupAsync(bakDir, true);
      const roDb = await openDatabaseAsync({ path: bakDir, readOnly: true });
      expect(roDb.readOnly).toBe(true);
      expect(roDb.get('t-1')?.fields['heading']).toBe('Rename me');
      roDb.close();

      const dumpReport = engine.admin.dump(dumpFile);
      expect(dumpReport.entities).toBe(1);
      const loadReport = loadDump(dumpFile, loadedDir);
      expect(loadReport.entities).toBe(1);

      const loadedEngine = openEngine({ path: loadedDir, readOnly: true });
      expect(loadedEngine.get('t-1')?.fields['heading']).toBe('Rename me');
      loadedEngine.admin.close();

      // Archive User type (dropping Task.assignee ref to User cleanly) and purge
      engine.create('User', { name: 'Gone' }, 'u-gone');
      const { assignee: _removedRef, ...taskFieldsWithoutAssignee } = gotSchema.types!['Task']!.fields!;
      const reduced: Schema = {
        types: {
          Task: {
            ...gotSchema.types!['Task']!,
            fields: taskFieldsWithoutAssignee,
          },
        },
        relations: gotSchema.relations ?? {},
      };
      engine.admin.setSchema(reduced);
      expect(engine.admin.purgeArchived('User')).toBe(1);

      engine.admin.close();

      // Calling openDatabase directly works cleanly too
      const reopened = openDatabase({ path: dir, readOnly: true });
      expect(reopened.get('t-1')?.fields['heading']).toBe('Rename me');
      reopened.close();
    } finally {
      fs.rmSync(dumpFile, { force: true });
    }
  });
});
