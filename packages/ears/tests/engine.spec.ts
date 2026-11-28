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
  type EarsOperation,
  type EarsOperationObserver,
  type EarsOperationResult,
  type Schema,
} from '../src/index.ts';
import { hasKnownErrorCode, toEarsError } from '../src/errors.ts';
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

    // P1-12: Async beforeCommit callbacks and invalid return types are rejected explicitly
    const unsubAsync = engine.beforeCommit(['Note'], (async () => {}) as unknown as () => void);
    let asyncHookErr: unknown;
    try {
      engine.create('Note', { title: 'Triggers async hook' }, 'n-async');
    } catch (err) {
      asyncHookErr = err;
    }
    expect(isEarsError(asyncHookErr)).toBe(true);
    expect((asyncHookErr as EarsError).code).toBe('ASYNC_CALLBACK_FORBIDDEN');
    expect((asyncHookErr as EarsError).message).toContain('synchronous');
    unsubAsync();
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

  it('supports nested transact(), RYOW, counters, runBeforeCommit: false, and EarsOperationObserver', () => {
    const events: Array<{ op: EarsOperation; res: EarsOperationResult }> = [];
    const observer: EarsOperationObserver = {
      start(op) {
        return (res) => {
          events.push({ op, res });
        };
      },
    };

    const handle = temps.open({ schema: baseSchema });
    const engine = createEarsEngine({ database: handle.database, observer });

    const watchSnapshots: number[] = [];
    const watch = engine.query.of('Task').watchCount((n) => watchSnapshots.push(n), { minGapMs: 0 });
    expect(watchSnapshots).toEqual([0]);

    let hookCount = 0;
    let hookSeq: number | undefined;
    const unsub = engine.beforeCommit(['Task'], (delta) => {
      hookCount++;
      hookSeq = delta.commitSeq;
      expect(engine.pendingCommitSeq()).toBe(delta.commitSeq);
      // RYOW inside beforeCommit sees outer + committed inner writes, NOT rolled-back inner writes
      expect(engine.get('t-outer')).not.toBeNull();
      expect(engine.get('t-inner-ok')).not.toBeNull();
      expect(engine.get('t-inner-abort')).toBeNull();
      engine.nextCounter('HOOK');
    });

    class DomainAbortError extends Error {
      readonly tag = 'domain';
    }

    const result = engine.transact(() => {
      const seq = engine.pendingCommitSeq();
      expect(typeof seq).toBe('number');
      expect(engine.nextCounter('TASK')).toBe(1);
      engine.create('Task', { title: 'Outer', done: false }, 't-outer');

      engine.transact(() => {
        expect(engine.nextCounter('TASK')).toBe(2);
        engine.create('Task', { title: 'Inner OK', done: false }, 't-inner-ok');
      });

      // Inner abort rolls back ONLY inner savepoint and preserves DomainAbortError verbatim!
      let caughtInner: unknown;
      try {
        engine.transact(() => {
          expect(engine.nextCounter('TASK')).toBe(3);
          engine.create('Task', { title: 'Inner Abort', done: false }, 't-inner-abort');
          expect(engine.get('t-inner-abort')).not.toBeNull();
          throw new DomainAbortError('inner rollback');
        });
      } catch (err) {
        caughtInner = err;
      }
      expect(caughtInner).toBeInstanceOf(DomainAbortError);
      expect((caughtInner as DomainAbortError).tag).toBe('domain');
      expect(engine.get('t-inner-abort')).toBeNull();
      expect(engine.getCounter('TASK')).toBe(2);
      expect(engine.query.of('Task').count()).toBe(2);
      expect(hookCount).toBe(0);
      return seq;
    });

    expect(engine.pendingCommitSeq()).toBeNull();
    expect(hookCount).toBe(1);
    expect(hookSeq).toBe(result);
    expect(engine.getCounter('TASK')).toBe(2);
    expect(engine.getCounter('HOOK')).toBe(1);

    // WatchPump is notified once outermost transact commits
    expect(engine.watches.settled()).toBe(false);
    engine.watches.flushSync(Date.now() + 100);
    expect(watchSnapshots).toEqual([0, 2]);
    expect(engine.watches.settled()).toBe(true);

    // Returning a Promise from transact aborts immediately with ASYNC_CALLBACK_FORBIDDEN
    let asyncErr: unknown;
    try {
      engine.transact((async () => {
        engine.nextCounter('TASK');
        engine.create('Task', { title: 'Should rollback', done: false }, 't-async');
      }) as unknown as () => void);
    } catch (err) {
      asyncErr = err;
    }
    expect(isEarsError(asyncErr)).toBe(true);
    expect((asyncErr as EarsError).code).toBe('ASYNC_CALLBACK_FORBIDDEN');
    expect(engine.get('t-async')).toBeNull();
    expect(engine.getCounter('TASK')).toBe(2);

    // { runBeforeCommit: false } skips hooks (for projection catch-up chunks)
    engine.transact(
      () => {
        engine.setCounter('IMPORT', 77);
        engine.create('Task', { title: 'No hook', done: true }, 't-no-hook');
      },
      { runBeforeCommit: false },
    );
    expect(hookCount).toBe(1);
    expect(engine.getCounter('IMPORT')).toBe(77);

    // Observer telemetry checks: unit spans, inUnit reads/writes, watch reads, aborted unit
    const unitSpans = events.filter((e) => e.op.kind === 'unit');
    expect(unitSpans.length).toBe(3); // outer ok, async aborted, runBeforeCommit:false ok
    expect(unitSpans[0]?.res.outcome).toBe('ok');
    expect(unitSpans[0]?.res.sequence).toBe(result);
    expect(unitSpans[1]?.res.outcome).toBe('aborted');
    expect(unitSpans[1]?.res.errorCode).toBe('ASYNC_CALLBACK_FORBIDDEN');
    expect(events.some((e) => e.op.kind === 'read' && e.op.inUnit)).toBe(true);
    expect(events.some((e) => e.op.kind === 'write' && e.op.inUnit)).toBe(true);
    expect(events.some((e) => e.op.kind === 'read' && e.op.watch)).toBe(true);

    // P2 #11: Read-only transact must NOT report an uncommitted tentative sequence to observer
    const beforeReadOnly = events.length;
    engine.transact(() => engine.get('t-outer'));
    const roUnit = events.slice(beforeReadOnly).find((e) => e.op.kind === 'unit');
    expect(roUnit?.res.outcome).toBe('ok');
    expect(roUnit?.res.sequence).toBeUndefined();

    // P1 #6: Calling engine.admin.close() inside transact rejects without killing WatchPump!
    engine.transact(() => {
      let closeErr: unknown;
      try {
        engine.admin.close();
      } catch (err) {
        closeErr = err;
      }
      expect(isEarsError(closeErr)).toBe(true);
      expect((closeErr as EarsError).code).toBe('INVALID_ARGUMENT');
      engine.create('Task', { title: 'Watch still live', done: false }, 't-after-close');
    });
    engine.watches.flushSync(Date.now() + 200);
    expect(watchSnapshots[watchSnapshots.length - 1]).toBe(4);

    // P1 #9: Throwing observer finisher is called ONCE (never double-finished on throw)
    let finishCalls = 0;
    const throwingObserver: EarsOperationObserver = {
      start() {
        return () => {
          finishCalls++;
          throw new Error('observer boom');
        };
      },
    };
    const obsHandle = temps.open({ schema: baseSchema });
    const obsEngine = createEarsEngine({ database: obsHandle.database, observer: throwingObserver });
    expect(() => obsEngine.get('missing')).toThrow('observer boom');
    expect(finishCalls).toBe(1);

    unsub();
    watch.stop();
  });

  it('preserves JS error messages from throwing beforeCommit hooks and clears pending N-API exceptions (P1-12) and avoids phantom sequence on rolled-back nested transact (P1-15)', () => {
    const events: Array<{ op: EarsOperation; res: EarsOperationResult }> = [];
    const observer: EarsOperationObserver = {
      start(op) {
        return (res) => events.push({ op, res });
      },
    };
    const handle = temps.open({ schema: baseSchema });
    const engine = createEarsEngine({ database: handle.database, observer });

    // P1-12 (a): Standard Error thrown inside beforeCommit preserves message + HOOK_ABORTED
    const unsubErr = engine.beforeCommit(['Task'], () => {
      throw new Error('custom domain violation');
    });
    let caughtBatch: unknown;
    try {
      engine.create('Task', { title: 'Blocked', done: false }, 't-blocked');
    } catch (e) {
      caughtBatch = e;
    }
    expect(isEarsError(caughtBatch)).toBe(true);
    expect((caughtBatch as EarsError).code).toBe('HOOK_ABORTED');
    expect((caughtBatch as EarsError).message).toContain('custom domain violation');

    // Also works inside transact() and leaves N-API env completely clean afterwards!
    let caughtTxn: unknown;
    try {
      engine.transact(() => {
        engine.create('Task', { title: 'Blocked in txn', done: false }, 't-blocked-txn');
      });
    } catch (e) {
      caughtTxn = e;
    }
    expect(isEarsError(caughtTxn)).toBe(true);
    expect((caughtTxn as EarsError).code).toBe('HOOK_ABORTED');
    expect((caughtTxn as EarsError).message).toContain('custom domain violation');
    unsubErr();

    // P1-12 (b): Non-Error string throw and hostile throwing `.message` getter both clear N-API cleanly!
    const unsubStr = engine.beforeCommit(['Task'], () => {
      // eslint-disable-next-line @typescript-eslint/only-throw-error
      throw 'string-hook-abort';
    });
    expect(() => engine.create('Task', { title: 'Str', done: false }, 't-str')).toThrowError(
      expect.objectContaining({ code: 'HOOK_ABORTED', message: expect.stringContaining('string-hook-abort') }),
    );
    unsubStr();

    const unsubHostile = engine.beforeCommit(['Task'], () => {
      const hostile = {
        get message(): string {
          throw new Error('getter explosion');
        },
        toString(): string {
          throw new Error('toString explosion');
        },
      };
      // eslint-disable-next-line @typescript-eslint/only-throw-error
      throw hostile;
    });
    expect(() => engine.create('Task', { title: 'Hostile', done: false }, 't-hostile')).toThrowError(
      expect.objectContaining({ code: 'HOOK_ABORTED' }),
    );
    unsubHostile();

    // Engine remains 100% healthy after all thrown hooks!
    engine.create('Task', { title: 'Healthy after hooks', done: false }, 't-ok');
    expect(engine.get('t-ok')?.fields['title']).toBe('Healthy after hooks');

    // P1-15: Nested transact() rolls back the ONLY write; outer catches & succeeds -> NO phantom sequence!
    const beforeNested = events.length;
    const outcome = engine.transact(() => {
      try {
        engine.transact(() => {
          engine.create('Task', { title: 'Rolled back', done: false }, 't-rb-only');
          throw new Error('inner rollback');
        });
      } catch {
        // Outer recovers and performs no writes of its own!
      }
      return 'recovered';
    });
    expect(outcome).toBe('recovered');
    expect(engine.get('t-rb-only')).toBeNull();
    const nestedUnit = events.slice(beforeNested).find((e) => e.op.kind === 'unit');
    expect(nestedUnit?.res.outcome).toBe('ok');
    expect(nestedUnit?.res.sequence).toBeUndefined();

    // P1-1: Throwing raw JS primitives in engine.transact() rethrows verbatim (never wrapped as INVALID_ARGUMENT)
    for (const prim of ['raw-string-error', 42, null, undefined]) {
      let caught: unknown = Symbol('unthrown');
      try {
        engine.transact(() => {
          engine.create('Task', { title: 'Should rollback', done: false }, 't-prim-rb');
          // eslint-disable-next-line @typescript-eslint/only-throw-error
          throw prim;
        });
      } catch (err) {
        caught = err;
      }
      expect(caught).toBe(prim);
      expect(engine.get('t-prim-rb')).toBeNull();
    }

    // P2-2: Empty engine.batch(() => {}) inside transact() must NOT report a phantom sequence to observer
    const beforeEmptyBatch = events.length;
    engine.transact(() => {
      engine.batch(() => {});
    });
    const emptyUnit = events.slice(beforeEmptyBatch).find((e) => e.op.kind === 'unit');
    expect(emptyUnit?.res.outcome).toBe('ok');
    expect(emptyUnit?.res.sequence).toBeUndefined();

    // ALREADY_LOADED is part of the unified ErrorCode / EarsErrorCode taxonomy
    expect(hasKnownErrorCode({ code: 'ALREADY_LOADED' })).toBe(true);
    const alreadyLoaded = toEarsError({ code: 'ALREADY_LOADED', message: 'another binary claimed process' });
    expect(alreadyLoaded.code).toBe('ALREADY_LOADED');
    expect(alreadyLoaded.message).toContain('another binary');

    // F3: Empty 0-op executeWireBatch inside transact() reads real op_count = 0 (no phantom sequence!)
    //     and non-empty executeWireBatch (including non-zero byteOffset slice) reports exact op_count.
    const beforeEmptyWire = events.length;
    engine.transact(() => {
      engine.executeWireBatch(encodeBatch(() => {}));
    });
    const emptyWireEvents = events.slice(beforeEmptyWire);
    const emptyWireWrite = emptyWireEvents.find((e) => e.op.kind === 'write');
    const emptyWireUnit = emptyWireEvents.find((e) => e.op.kind === 'unit');
    expect(emptyWireWrite?.op.ops).toBe(0);
    expect(emptyWireUnit?.res.outcome).toBe('ok');
    expect(emptyWireUnit?.res.sequence).toBeUndefined();

    const twoOpFrame = encodeBatch((b) => {
      b.create('Task', { title: 'w1', done: false }, 'f3-1');
      b.create('Task', { title: 'w2', done: true }, 'f3-2');
    });
    const padded = new Uint8Array(twoOpFrame.byteLength + 8);
    padded.set(twoOpFrame, 4);
    const slicedView = padded.subarray(4, 4 + twoOpFrame.byteLength);
    const beforeTwoWire = events.length;
    engine.executeWireBatch(slicedView);
    const twoWireWrite = events.slice(beforeTwoWire).find((e) => e.op.kind === 'write');
    expect(twoWireWrite?.op.ops).toBe(2);

    // F4: Hostile throwing `.then` getter on returned object is wrapped as EarsError('INVALID_ARGUMENT')
    //     while real Promises/thenables preserve EarsError('ASYNC_CALLBACK_FORBIDDEN') in both encodeBatch & batch.
    const hostileThenable = Object.defineProperty({}, 'then', {
      get() {
        throw new Error('hostile .then getter');
      },
    });
    expect(() =>
      encodeBatch((() => hostileThenable) as unknown as (b: Parameters<typeof encodeBatch>[0] extends (x: infer W) => void ? W : never) => void),
    ).toThrowError(expect.objectContaining({ code: 'INVALID_ARGUMENT', message: expect.stringContaining('hostile .then') }));
    expect(() =>
      engine.batch((() => hostileThenable) as unknown as Parameters<typeof engine.batch>[0]),
    ).toThrowError(expect.objectContaining({ code: 'INVALID_ARGUMENT', message: expect.stringContaining('hostile .then') }));
    expect(() =>
      encodeBatch((async () => {}) as unknown as Parameters<typeof encodeBatch>[0]),
    ).toThrowError(expect.objectContaining({ code: 'ASYNC_CALLBACK_FORBIDDEN' }));
    expect(() =>
      engine.batch((async () => {}) as unknown as Parameters<typeof engine.batch>[0]),
    ).toThrowError(expect.objectContaining({ code: 'ASYNC_CALLBACK_FORBIDDEN' }));

    // Non-Uint8Array payload with an observer still reports EarsError('INVALID_ARGUMENT') without TypeError on byteLength.
    expect(() => engine.executeWireBatch(null as unknown as Uint8Array)).toThrowError(
      expect.objectContaining({ code: 'INVALID_ARGUMENT' }),
    );
  });

  it('PR #268 Batch 3: crash-proof toEarsError/hasKnownErrorCode, beforeCommit ASYNC_CALLBACK_FORBIDDEN, flushSync in txn, and finally notifyMutation', async () => {
    const handle = temps.open({ schema: baseSchema });
    const engine = createEarsEngine({ database: handle.database });

    // 1. Crash-proof `toEarsError` and `hasKnownErrorCode` against `Object.create(null)` & throwing getters.
    const nullProto = Object.create(null) as unknown;
    expect(hasKnownErrorCode(nullProto)).toBe(false);
    const nullErr = toEarsError(nullProto);
    expect(nullErr).toBeInstanceOf(EarsError);
    expect(nullErr.code).toBe('INTERNAL');
    expect(nullErr.message).toBe('[unstringifiable error]');

    const hostileGetters = Object.defineProperties({}, {
      code: {
        get() {
          throw new Error('code getter boom');
        },
      },
      message: {
        get() {
          throw new Error('message getter boom');
        },
      },
      toString: {
        value() {
          throw new Error('toString boom');
        },
      },
    });
    expect(hasKnownErrorCode(hostileGetters)).toBe(false);
    const hostileErr = toEarsError(hostileGetters, 'INVALID_ARGUMENT');
    expect(hostileErr.code).toBe('INVALID_ARGUMENT');
    expect(hostileErr.message).toBe('[unstringifiable error]');

    // Revoked Proxy, hostile `getPrototypeOf` Proxy, and prototype-spoofed `EarsError` must never crash!
    const { proxy: revokedProxy, revoke } = Proxy.revocable({}, {});
    revoke();
    const hostileProtoProxy = new Proxy({}, {
      getPrototypeOf() {
        throw new Error('getPrototypeOf boom');
      },
    });
    const spoofedEarsErr = Object.create(EarsError.prototype) as Record<string, unknown>;
    Object.defineProperty(spoofedEarsErr, 'code', {
      get() {
        throw new Error('spoofed code boom');
      },
    });
    for (const hostile of [revokedProxy, hostileProtoProxy, spoofedEarsErr]) {
      expect(isEarsError(hostile)).toBe(false);
      expect(hasKnownErrorCode(hostile)).toBe(false);
      const wrapped = toEarsError(hostile);
      expect(isEarsError(wrapped)).toBe(true);
      expect(wrapped.code).toBe('INTERNAL');
    }

    // 2. `beforeCommit` callable thenable, async batch builder, AND stateful `.then` getter on Uint8Array
    //    all preserve `ASYNC_CALLBACK_FORBIDDEN`.
    const callableThenable = Object.assign(() => {}, {
      then() {},
    });
    const unsubCallable = engine.beforeCommit(['Task'], () => callableThenable as unknown as void);
    expect(() => engine.create('Task', { title: 'callable-thenable', done: false })).toThrowError(
      expect.objectContaining({ code: 'ASYNC_CALLBACK_FORBIDDEN' }),
    );
    unsubCallable();

    const unsubStatefulThen = engine.beforeCommit(['Task'], () => {
      const payload = encodeBatch((b) => { b.create('Task', { title: 'derived', done: false }); });
      let reads = 0;
      Object.defineProperty(payload, 'then', {
        get() {
          return reads++ === 0 ? () => {} : undefined;
        },
      });
      return payload;
    });
    expect(() => engine.create('Task', { title: 'stateful-then', done: false })).toThrowError(
      expect.objectContaining({ code: 'ASYNC_CALLBACK_FORBIDDEN' }),
    );
    unsubStatefulThen();

    // Prototype-spoofed Uint8Array passed to executeWireBatch throws EarsError('INVALID_ARGUMENT'), not unwrapped TypeError.
    expect(() => engine.executeWireBatch(Object.create(Uint8Array.prototype) as Uint8Array)).toThrowError(
      expect.objectContaining({ code: 'INVALID_ARGUMENT' }),
    );

    const unsubAsyncBuilder = engine.beforeCommit(
      ['Task'],
      () => (async () => {}) as unknown as (b: Parameters<typeof encodeBatch>[0] extends (x: infer W) => void ? W : never) => void,
    );
    expect(() => engine.create('Task', { title: 'async-builder', done: false })).toThrowError(
      expect.objectContaining({ code: 'ASYNC_CALLBACK_FORBIDDEN' }),
    );
    unsubAsyncBuilder();

    // 3. `watches.flushSync()` inside `transact()` fails fast with `INVALID_ARGUMENT` WITHOUT unregistering healthy watches!
    const watchCounts: number[] = [];
    const wh = engine.query.of('Task').watchCount((c) => watchCounts.push(c), {
      minGapMs: 0,
      adaptiveMultiplier: 0,
    });
    expect(watchCounts).toEqual([0]);
    engine.transact(() => {
      engine.create('Task', { title: 'inside-txn', done: false }, 'b3-txn-1');
      expect(() => engine.watches.flushSync()).toThrowError(
        expect.objectContaining({ code: 'INVALID_ARGUMENT' }),
      );
      expect(wh.active).toBe(true);
    });
    expect(wh.active).toBe(true);
    engine.watches.flushSync();
    expect(watchCounts).toEqual([0, 1]);
    wh.stop();

    // 4. `flush()`, `flushAsync()`, and `driveBackgroundWork()` notify watches in `finally` even when throwing after partial progress.
    let notified = 0;
    const rawDb = handle.database;
    const wrappedDb = new Proxy(rawDb, {
      get(target, prop, receiver) {
        if (prop === 'tickWatches') {
          return (nowMs: number) => {
            notified++;
            return target.tickWatches(nowMs);
          };
        }
        if (prop === 'flush') {
          return () => {
            throw new EarsError('IO', 'flush partial failure');
          };
        }
        if (prop === 'flushAsync') {
          return async () => {
            throw new EarsError('IO', 'flushAsync partial failure');
          };
        }
        if (prop === 'driveBackgroundWork') {
          return () => {
            throw new EarsError('IO', 'bg partial failure');
          };
        }
        const value = Reflect.get(target, prop, receiver);
        return typeof value === 'function' ? (value as (...a: unknown[]) => unknown).bind(target) : value;
      },
    });
    const eng2 = createEarsEngine({ database: wrappedDb });
    const wh2 = eng2.query.of('Task').watchCount(() => {}, { minGapMs: 0 });
    notified = 0;
    expect(() => eng2.admin.flush()).toThrowError(expect.objectContaining({ code: 'IO' }));
    expect(notified).toBeGreaterThanOrEqual(1);
    notified = 0;
    await expect(eng2.admin.flushAsync()).rejects.toMatchObject({ code: 'IO' });
    expect(notified).toBeGreaterThanOrEqual(1);
    notified = 0;
    expect(() => eng2.admin.driveBackgroundWork(10)).toThrowError(expect.objectContaining({ code: 'IO' }));
    expect(notified).toBeGreaterThanOrEqual(1);
    wh2.stop();

    // Throwing clock.now() inside `finally { pump.notifyMutation(); }` must NEVER mask the original DB error!
    let failNotifyClock = false;
    const eng3 = createEarsEngine({
      database: wrappedDb,
      clock: {
        now() {
          if (failNotifyClock) throw new Error('clock boom during notify');
          return 1_000;
        },
        setTimeout: globalThis.setTimeout,
        clearTimeout: (h) => globalThis.clearTimeout(h as ReturnType<typeof globalThis.setTimeout>),
      },
    });
    const wh3 = eng3.query.of('Task').watchCount(() => {}, { minGapMs: 0 });
    failNotifyClock = true;
    expect(() => eng3.admin.flush()).toThrowError(
      expect.objectContaining({ code: 'IO', message: 'flush partial failure' }),
    );
    await expect(eng3.admin.flushAsync()).rejects.toMatchObject({
      code: 'IO',
      message: 'flushAsync partial failure',
    });
    wh3.stop();
  });
});
