// Durability, nested durability propagation, savepoint rollback, rollback error preservation, and observation
// classification under `NOT_DURABLE` and `MAP_FULL`.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { createEarsEngine, EarsError, type EarsDatabase, type EarsOperationResult } from '../../src/index.ts';
import { openDatabase, type DatabaseOptions } from '../../src/lmdb/index.ts';

const SCHEMA = {
  types: { Task: { required: [] } },
  relations: { blocks: {} },
  roles: [],
};

const opened: Array<{ database: EarsDatabase; dir: string }> = [];

/** Opens a real elmdb database with custom `DatabaseOptions` and schedules cleanup after each test */
function openTemp(options: Omit<DatabaseOptions, 'path' | 'schema'> = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-durability-'));
  const database = openDatabase({ path: dir, schema: SCHEMA, ...options });
  opened.push({ database, dir });
  return {
    database,
    dir,
    syncs: () => (database as unknown as { syncs: number }).syncs,
  };
}

afterEach(() => {
  while (opened.length > 0) {
    const { database, dir } = opened.pop()!;
    try { if (database.isOpen) database.close(); } catch { /* best effort */ }
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

describe('durability and nested transact contract', () => {
  it('propagates durable: true from a committed nested transact to the outermost commit', () => {
    // Long idle/interval windows prevent background group commit from syncing during the test
    const { database, syncs } = openTemp({ syncs: true, syncIdleMs: 60_000, syncIntervalMs: 60_000 });
    const e = createEarsEngine({ database });

    const before = syncs();
    e.transact(() => {
      e.create('Task', { title: 'outer' });
      e.transact(() => {
        e.create('Task', { title: 'inner-durable' });
      }, { durable: true });
      // Nothing has synced yet because only the outermost unit commits
      expect(syncs()).toBe(before);
    });
    expect(syncs()).toBe(before + 1);
    expect(e.query('Task').count()).toBe(2);

    // Even a 3-level nested durable: true with no writes of its own makes the enclosing unit's writes durable
    const before2 = syncs();
    e.transact(() => {
      e.create('Task', { title: 'outer-write' });
      e.transact(() => {
        e.transact(() => {
          // Read-only inner durable request
          expect(e.query('Task').count()).toBe(3);
        }, { durable: true });
      });
    });
    expect(syncs()).toBe(before2 + 1);
  });

  it('does not leak durable: true from a nested transact that throws and rolls back', () => {
    const { database, syncs } = openTemp({ syncs: true, syncIdleMs: 60_000, syncIntervalMs: 60_000 });
    const e = createEarsEngine({ database });

    const before = syncs();
    e.transact(() => {
      e.create('Task', { title: 'outer-non-durable' });
      expect(() => e.transact(() => {
        e.create('Task', { title: 'rolled-back-durable' });
        throw new Error('abort inner');
      }, { durable: true })).toThrow('abort inner');
    });
    expect(syncs(), 'rolled-back nested durable unit must not force outer sync').toBe(before);
    expect(e.query('Task').pickAll().map((r) => r.title)).toEqual(['outer-non-durable']);

    // Middle level that received inner durable: true and then threw must not leak durability to outer either
    e.transact(() => {
      e.create('Task', { title: 'outer-2' });
      expect(() => e.transact(() => {
        e.transact(() => {
          e.create('Task', { title: 'inner-3' });
        }, { durable: true });
        throw new Error('abort middle');
      })).toThrow('abort middle');
    });
    expect(syncs()).toBe(before);
    expect(e.query('Task').pickAll().map((r) => r.title)).toEqual(['outer-non-durable', 'outer-2']);
  });

  it('supports admin.durable() and admin.transactWithoutHook(fn, { durable: true })', async () => {
    const { database, syncs } = openTemp({ syncs: true, syncIdleMs: 60_000, syncIntervalMs: 60_000 });
    const e = createEarsEngine({ database });

    let hookRuns = 0;
    e.admin.beforeCommit({ wants: () => true, run: () => { hookRuns++; } });

    // Non-durable commit leaves syncs unchanged until admin.durable() awaits a sync
    const before = syncs();
    e.create('Task', { title: 'buffered' });
    expect(hookRuns).toBe(1);
    expect(syncs()).toBe(before);
    await e.admin.durable();
    expect(syncs()).toBe(before + 1);

    // transactWithoutHook with durable: true skips the hook and syncs immediately
    const beforeHookless = syncs();
    e.admin.transactWithoutHook(() => {
      e.create('Task', { title: 'hookless-durable' });
    }, { durable: true });
    expect(hookRuns).toBe(1);
    expect(syncs()).toBe(beforeHookless + 1);

    // admin.durable() rejects with CLOSED once the database is closed
    database.close();
    await expect(e.admin.durable()).rejects.toMatchObject({ code: 'CLOSED' });
  });

  it('maps async and synchronous errors from database.durable() through crossing', async () => {
    const { database } = openTemp({ syncs: false });
    let mode: 'reject-mapped' | 'throw-mapped' = 'reject-mapped';
    const wrapped: EarsDatabase = new Proxy(database, {
      get(target, key) {
        if (key === 'durable') {
          return () => {
            const raw = Object.assign(new Error('bad value'), { code: 'VALUE_INVALID' });
            if (mode === 'throw-mapped') throw raw;
            return Promise.reject(raw);
          };
        }
        const value = Reflect.get(target, key, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });
    const e = createEarsEngine({ database: wrapped });

    await expect(e.admin.durable()).rejects.toSatisfy((err: unknown) => err instanceof EarsError && err.code === 'VALUE_INVALID');
    mode = 'throw-mapped';
    await expect(e.admin.durable()).rejects.toSatisfy((err: unknown) => err instanceof EarsError && err.code === 'VALUE_INVALID');
  });

  it('forwards syncEachCommit, syncIdleMs, syncIntervalMs, and mapSizeBytes through openDatabase', async () => {
    const { database, syncs } = openTemp({
      syncs: true,
      syncEachCommit: true,
      syncIdleMs: 60_000,
      syncIntervalMs: 60_000,
      mapSizeBytes: 16 * 1024 * 1024,
    });
    const e = createEarsEngine({ database });
    const before = syncs();
    e.create('Task', { title: 'synced-each-commit' });
    // Under syncEachCommit every commit syncs directly, so group commit runs no sync even when durable() is awaited
    // (if syncEachCommit were not forwarded, durable() would trigger group commit and increment syncs)
    await e.admin.durable();
    expect(syncs()).toBe(before);
    expect(e.query('Task').count()).toBe(1);

    // Invalid option values are forwarded and refused by elmdb
    for (const bad of [{ mapSizeBytes: 0 }, { syncIdleMs: -1 }, { syncIntervalMs: -1 }]) {
      const badDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-bad-opts-'));
      try {
        expect(() => openDatabase({ path: badDir, schema: SCHEMA, ...bad })).toThrow(expect.objectContaining({ code: 'VALUE_INVALID' }));
      } finally {
        fs.rmSync(badDir, { recursive: true, force: true });
      }
    }
  });
});

describe('savepoint rollback and poisoned unit error preservation', () => {
  it('restores prior admin.setCheckpoint on nested savepoint rollback and emits zero watch invalidations', async () => {
    const { database } = openTemp({ syncs: false });
    const e = createEarsEngine({ database });

    e.transact(() => {
      e.admin.setCheckpoint('cursor', { seq: 1 });
    });
    expect(e.admin.checkpoint('cursor')).toEqual({ seq: 1 });

    let watchRuns = 0;
    const handle = e.watch(() => {
      watchRuns++;
      return e.query('Task').count();
    }, () => {});
    await e.admin.watchesSettled();
    const runsBefore = watchRuns;

    // Nested transact sets a new checkpoint and then rolls back inside an outer unit with no other writes
    e.transact(() => {
      expect(() => e.transact(() => {
        e.admin.setCheckpoint('cursor', { seq: 99 });
        expect(e.admin.checkpoint('cursor')).toEqual({ seq: 99 });
        throw new Error('rollback checkpoint');
      })).toThrow('rollback checkpoint');
      expect(e.admin.checkpoint('cursor')).toEqual({ seq: 1 });
    });

    await e.admin.watchesSettled();
    expect(e.admin.checkpoint('cursor')).toEqual({ seq: 1 });
    expect(watchRuns, 'rolled-back checkpoint emits zero watch invalidations').toBe(runsBefore);

    // Also verify when outer DOES keep a write, the inner rolled-back checkpoint does not survive
    e.transact(() => {
      e.admin.setCheckpoint('cursor', { seq: 2 });
      expect(() => e.transact(() => {
        e.admin.setCheckpoint('cursor', { seq: 100 });
        throw new Error('rollback inner checkpoint');
      })).toThrow('rollback inner checkpoint');
      expect(e.admin.checkpoint('cursor')).toEqual({ seq: 2 });
    });
    expect(e.admin.checkpoint('cursor')).toEqual({ seq: 2 });
    handle.stop();
  });

  it('preserves the original MAP_FULL error when nested rollbackTo also fails, leaking zero writes or watch notifications', async () => {
    const { database } = openTemp({ syncs: false });
    const mapFull = Object.assign(new Error('MDB_MAP_FULL: Environment mapsize limit reached'), { code: 'MAP_FULL' });
    const rollbackErr = Object.assign(new Error('MDB_MAP_FULL during rollbackTo'), { code: 'MAP_FULL' });
    let triggerMapFull = false;

    const wrapped: EarsDatabase = new Proxy(database, {
      get(target, key) {
        if (key === 'begin') {
          return () => {
            const unit = target.begin();
            let poisoned = false;
            return new Proxy(unit, {
              get(held, member) {
                if (member === 'write') {
                  return (batch: Uint8Array) => {
                    if (triggerMapFull) {
                      poisoned = true;
                      throw mapFull;
                    }
                    return held.write(batch);
                  };
                }
                if (member === 'rollbackTo') {
                  return (depth: number) => {
                    if (poisoned) throw rollbackErr;
                    return held.rollbackTo(depth);
                  };
                }
                const value = Reflect.get(held, member, held);
                return typeof value === 'function' ? value.bind(held) : value;
              },
            });
          };
        }
        const value = Reflect.get(target, key, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });

    const e = createEarsEngine({ database: wrapped });
    let watchRuns = 0;
    const handle = e.watch(() => {
      watchRuns++;
      return e.query('Task').ids();
    }, () => {});
    await e.admin.watchesSettled();
    const runsBefore = watchRuns;

    let caught: unknown;
    try {
      e.transact(() => {
        e.create('Task', { title: 'outer-before-mapfull' });
        e.transact(() => {
          triggerMapFull = true;
          e.create('Task', { title: 'inner-hits-mapfull' });
        });
      });
    } catch (error) {
      caught = error;
    }

    expect(caught).toBeInstanceOf(AggregateError);
    const aggregate = caught as AggregateError & { code?: string };
    expect(aggregate.errors).toEqual([mapFull, rollbackErr]);
    expect(aggregate.cause).toBe(mapFull);
    expect(aggregate.code).toBe('MAP_FULL');

    await e.admin.watchesSettled();
    expect(e.query('Task').ids(), 'outer unit aborted cleanly with zero leaked writes').toEqual([]);
    expect(watchRuns, 'aborted unit emitted zero watch invalidations').toBe(runsBefore);
    handle.stop();
  });

  it('preserves the original error when outermost unit.abort() also throws on failure', async () => {
    const { database } = openTemp({ syncs: false });
    const mapFull = Object.assign(new Error('map full'), { code: 'MAP_FULL' });
    const abortErr = Object.assign(new Error('abort failed'), { code: 'LMDB' });

    const wrapped: EarsDatabase = new Proxy(database, {
      get(target, key) {
        if (key === 'begin') {
          return () => {
            const unit = target.begin();
            return new Proxy(unit, {
              get(held, member) {
                if (member === 'abort') {
                  return () => {
                    held.abort();
                    throw abortErr;
                  };
                }
                const value = Reflect.get(held, member, held);
                return typeof value === 'function' ? value.bind(held) : value;
              },
            });
          };
        }
        const value = Reflect.get(target, key, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });

    const e = createEarsEngine({ database: wrapped });
    let caught: unknown;
    try {
      e.transact(() => {
        e.create('Task', { title: 'will-abort' });
        throw mapFull;
      });
    } catch (error) {
      caught = error;
    }
    expect(caught).toBeInstanceOf(AggregateError);
    expect((caught as AggregateError).errors).toEqual([mapFull, abortErr]);
    expect((caught as AggregateError & { code?: string }).code).toBe('MAP_FULL');
    expect(e.query('Task').ids()).toEqual([]);
  });
});

describe('observation committed-not-durable classification', () => {
  it('classifies unit commit, standalone write, and bulkLoad NOT_DURABLE as committed-not-durable even when database.sequence throws', () => {
    const { database } = openTemp({ syncs: false });
    let failSequence = false;
    let failCommitNotDurable = false;
    let failWriteNotDurable = false;
    let failBulkLoadNotDurable = false;

    const wrapped: EarsDatabase = new Proxy(database, {
      get(target, key) {
        if (key === 'sequence' && failSequence) throw new Error('sequence unavailable');
        if (key === 'write') {
          return (batch: Uint8Array, durable?: boolean | null) => {
            const res = target.write(batch, durable);
            if (failWriteNotDurable) {
              failSequence = true;
              throw Object.assign(new Error('sync failed on write'), { code: 'NOT_DURABLE' });
            }
            return res;
          };
        }
        if (key === 'bulkLoad') {
          return (loads: Uint8Array) => {
            target.bulkLoad(loads);
            if (failBulkLoadNotDurable) {
              failSequence = true;
              throw Object.assign(new Error('sync failed on load'), { code: 'NOT_DURABLE' });
            }
          };
        }
        if (key === 'begin') {
          return () => {
            const unit = target.begin();
            return new Proxy(unit, {
              get(held, member) {
                if (member === 'commit') {
                  return (durable?: boolean | null) => {
                    const res = held.commit(durable);
                    if (failCommitNotDurable) {
                      failSequence = true;
                      throw Object.assign(new Error('sync failed on commit'), { code: 'NOT_DURABLE' });
                    }
                    return res;
                  };
                }
                const value = Reflect.get(held, member, held);
                return typeof value === 'function' ? value.bind(held) : value;
              },
            });
          };
        }
        const value = Reflect.get(target, key, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });

    const e = createEarsEngine({ database: wrapped });
    const observed: Array<{ kind: string; result: EarsOperationResult }> = [];
    e.admin.observe({
      start(op) {
        return (result) => { observed.push({ kind: op.kind, result }); };
      },
    });

    // 1. Durable unit commit fails with NOT_DURABLE and database.sequence throws afterward
    failCommitNotDurable = true;
    expect(() => e.transact(() => {
      e.create('Task', { title: 'committed-unit' });
    }, { durable: true })).toThrow(expect.objectContaining({ code: 'NOT_DURABLE' }));
    failCommitNotDurable = false;
    failSequence = false;

    const unitEvent = observed.find((entry) => entry.kind === 'unit');
    expect(unitEvent?.result).toMatchObject({
      outcome: 'committed-not-durable',
      errorCode: 'NOT_DURABLE',
      sequence: undefined,
    });
    expect(e.query('Task').count()).toBe(1);

    // 2. Standalone write fails with NOT_DURABLE and database.sequence throws afterward
    observed.length = 0;
    failWriteNotDurable = true;
    expect(() => e.create('Task', { title: 'committed-write' })).toThrow(expect.objectContaining({ code: 'NOT_DURABLE' }));
    failWriteNotDurable = false;
    failSequence = false;

    const writeEvent = observed.find((entry) => entry.kind === 'write');
    expect(writeEvent?.result).toMatchObject({
      outcome: 'committed-not-durable',
      errorCode: 'NOT_DURABLE',
      sequence: undefined,
    });
    expect(e.query('Task').count()).toBe(2);

    // 3. bulkLoad fails with NOT_DURABLE and database.sequence throws afterward
    observed.length = 0;
    failBulkLoadNotDurable = true;
    expect(() => e.admin.bulkLoad(() => {
      e.admin.bulkLoadAttr('Task-loaded' as never, 'title' as never, 'committed-load');
    })).toThrow(expect.objectContaining({ code: 'NOT_DURABLE' }));
    failBulkLoadNotDurable = false;
    failSequence = false;

    const loadEvent = observed.find((entry) => entry.kind === 'load');
    expect(loadEvent?.result).toMatchObject({
      outcome: 'committed-not-durable',
      errorCode: 'NOT_DURABLE',
      sequence: undefined,
    });

    // 4. Uncommitted NOT_DURABLE thrown by user code inside transact stays classified as 'error'
    observed.length = 0;
    expect(() => e.transact(() => {
      e.create('Task', { title: 'not-committed' });
      throw Object.assign(new Error('user threw NOT_DURABLE before commit'), { code: 'NOT_DURABLE' });
    })).toThrow(expect.objectContaining({ code: 'NOT_DURABLE' }));
    const abortedUnit = observed.find((entry) => entry.kind === 'unit');
    expect(abortedUnit?.result.outcome).toBe('error');

    // 5. With a watch listener registered, if database.sequence throws during committedEverything(),
    //    NOT_DURABLE is still preserved as an AggregateError with code NOT_DURABLE and classified committed-not-durable
    const handle = e.watch(() => e.query('Task').count(), () => {});
    observed.length = 0;
    failCommitNotDurable = true;
    let watchRefreshErr: unknown;
    try {
      e.transact(() => {
        e.create('Task', { title: 'committed-with-watch' });
      }, { durable: true });
    } catch (err) {
      watchRefreshErr = err;
    }
    failCommitNotDurable = false;
    failSequence = false;
    handle.stop();

    expect(watchRefreshErr).toBeInstanceOf(AggregateError);
    expect((watchRefreshErr as AggregateError & { code?: string }).code).toBe('NOT_DURABLE');
    expect(observed.find((entry) => entry.kind === 'unit')?.result.outcome).toBe('committed-not-durable');
  });

  it('extracts EarsError.code when wrapped in AggregateError by a rollback failure', () => {
    const { database } = openTemp({ syncs: false });
    const rollbackErr = Object.assign(new Error('rollback failed'), { code: 'LMDB' });
    const wrapped: EarsDatabase = new Proxy(database, {
      get(target, key) {
        if (key === 'begin') {
          return () => {
            const unit = target.begin();
            return new Proxy(unit, {
              get(held, member) {
                if (member === 'rollbackTo') return () => { throw rollbackErr; };
                const value = Reflect.get(held, member, held);
                return typeof value === 'function' ? value.bind(held) : value;
              },
            });
          };
        }
        const value = Reflect.get(target, key, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    });

    const e = createEarsEngine({ database: wrapped });
    const observed: Array<{ kind: string; result: EarsOperationResult }> = [];
    e.admin.observe({
      start(op) {
        return (result) => { observed.push({ kind: op.kind, result }); };
      },
    });

    let caught: unknown;
    try {
      e.transact(() => {
        e.create('Task', { title: 'outer' });
        e.transact(() => {
          e.write('Task-missing' as never).set('title', 'nope');
        });
      });
    } catch (error) {
      caught = error;
    }

    expect(caught).toBeInstanceOf(AggregateError);
    expect((caught as AggregateError & { code?: string }).code).toBe('NO_SUCH_ENTITY');
    const unitEvents = observed.filter((entry) => entry.kind === 'unit');
    expect(unitEvents.length).toBe(2);
    for (const event of unitEvents) {
      expect(event.result.outcome).toBe('error');
      expect(event.result.errorCode).toBe('NO_SUCH_ENTITY');
    }
  });
});
