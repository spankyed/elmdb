// Full v2 Database binding lifecycle: open/openAsync, schema + O(1) rename + archive/purge,
// binary wire batches, beforeCommit savepoint rollback & cascade, backup, dump/load, check/rebuild/flush.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { Database } = require('../index.js');
const { buildWireBatch } = require('../client.js');

const baseSchema = {
  types: {
    Note: {
      fields: {
        title: { type: 'string' },
        tag: { type: 'string', optional: true },
        audit: { type: 'boolean', optional: true },
      },
      indexes: {
        byTitle: { kind: 'value', fields: ['title'], unique: true },
      },
    },
    Audit: {
      fields: {
        noteId: { type: 'string' },
      },
    },
  },
  relations: {
    refs: { endpoints: [{ source: 'Note', target: 'Note' }] },
  },
};

test('open, schema evolution, O(1) rename, archive & purge, backup, dump/load, and async ops', async (t) => {
  const tmp = mkdtempSync(join(tmpdir(), 'elmdb-v2-db-'));
  const dir = join(tmp, 'main');
  const db = await Database.openAsync(dir, { schema: baseSchema, durability: 'strict', buildPolicy: 'eager' });
  t.after(() => {
    try { db.close(); } finally { rmSync(tmp, { recursive: true, force: true }); }
  });

  assert.equal(db.readOnly, false);
  assert.equal(db.path, dir);
  assert.equal(db.openReport().status, 'fresh');

  // Execute batch + enforce unique index
  db.executeWireBatch(buildWireBatch((b) => {
    const a = b.create('Note', { title: 'Unique-1', tag: 'a' }, 'n1');
    const c = b.create('Note', { title: 'Unique-2', tag: 'b' }, 'n2');
    b.link(a, 'refs', c);
  }));

  assert.throws(
    () => db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'Unique-1' }, 'n3'); })),
    (err) => err.code === 'UNIQUE_VIOLATION',
  );

  // O(1) rename field
  const epoch = db.renameField('Note', 'tag', 'label');
  assert.ok(epoch > 1);
  assert.equal(db.get('n1').fields.label, 'a');

  // beforeCommit: throwing aborts cleanly via native savepoint; returning a wire batch cascades derived writes!
  let abortFlag = true;
  const hookId = db.registerBeforeCommit(['Note'], (ev) => {
    if (abortFlag) throw new Error('rejected by policy');
    const created = ev.entities.filter((e) => e.op === 'create');
    if (created.length === 0) return null;
    return buildWireBatch((b) => {
      for (const c of created) b.create('Audit', { noteId: c.id }, `audit-${c.id}`);
    });
  });

  assert.throws(
    () => db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'Rolled-back' }, 'n-bad'); })),
    (err) => err.code === 'HOOK_ABORTED',
  );
  assert.equal(db.get('n-bad'), null);

  abortFlag = false;
  db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'Cascaded' }, 'n-good'); }));
  assert.ok(db.get('n-good'));
  assert.equal(db.get('audit-n-good').fields.noteId, 'n-good');
  assert.equal(db.unregisterBeforeCommit(hookId), true);

  // Check, rebuild, flush (sync + async)
  await db.flushAsync();
  const chk = await db.checkAsync();
  assert.equal(chk.clean, true);
  await db.rebuildAsync();
  assert.equal(db.check().clean, true);
  assert.ok(db.indexStatus().every((s) => s.state === 'ready'));
  assert.equal(db.stats().typeCounts.Note, 3);

  // Backup + overwrite guard + async backup
  const backupDir = join(tmp, 'backup');
  await db.backupAsync(backupDir, false);
  assert.throws(() => db.backup(backupDir, false), (err) => err.code === 'ALREADY_EXISTS');
  db.backup(backupDir, true);

  // Dump + Database.load
  const dumpFile = join(tmp, 'dump.json');
  const dumpRep = db.dump(dumpFile);
  assert.equal(dumpRep.entities, 4); // 3 Notes + 1 Audit
  assert.equal(dumpRep.relations, 1);

  const loadedDir = join(tmp, 'loaded');
  const loadRep = Database.load(dumpFile, loadedDir);
  assert.equal(loadRep.entities, 4);
  const loaded = Database.open(loadedDir, { readOnly: true });
  assert.equal(loaded.get('n1').fields.title, 'Unique-1');
  assert.equal(loaded.check().clean, true);
  loaded.close();

  // P0-1: db.setSchema(db.getSchema()) roundtrips cleanly (including relations & Ref onDelete)!
  const currentSchema = db.getSchema();
  db.setSchema(currentSchema);

  // Archive Audit type and purgeArchived
  db.setSchema({ types: { Note: currentSchema.types.Note }, relations: currentSchema.relations });
  assert.equal(db.purgeArchived('Audit'), 1);
  assert.equal(db.get('audit-n-good'), null);

  // P0-3 & P1-6: Re-entrant beforeCommit hook + per-round deltas + edge sourceId/targetId + readOnly rejection
  let reentrantRan = false;
  let seenEdge = null;
  const h2 = db.registerBeforeCommit(null, (ev) => {
    if (ev.edges.length > 0) seenEdge = ev.edges[0];
    // Re-entrant unregister/register inside callback must NOT panic with BorrowMutError!
    if (!reentrantRan) {
      reentrantRan = true;
      db.unregisterBeforeCommit(h2);
    }
    return null;
  });
  db.executeWireBatch(buildWireBatch((b) => {
    const x = b.create('Note', { title: 'EdgeSrc' }, 'n-src');
    const y = b.create('Note', { title: 'EdgeDst' }, 'n-dst');
    b.link(x, 'refs', y);
  }));
  assert.equal(reentrantRan, true);
  assert.equal(seenEdge.sourceId, 'n-src');
  assert.equal(seenEdge.targetId, 'n-dst');

  // P0-5: Backup opens with clean openReport status (verified synced.v2 sidecar)
  const bakCheck = Database.open(backupDir, { readOnly: true });
  assert.equal(bakCheck.openReport().status, 'clean');
  bakCheck.close();

  // P1-8: Reject readOnly + schema and out-of-range u32 maxDbs before touching disk
  assert.throws(
    () => Database.open(dir, { readOnly: true, schema: baseSchema }),
    (err) => err.code === 'READ_ONLY',
  );
  assert.throws(
    () => Database.open(dir, { maxDbs: 2 ** 32 }),
    (err) => err.code === 'INVALID_ARGUMENT',
  );

  db.close();
  assert.throws(() => db.stats(), (err) => err.code === 'CLOSED');
});

test('transact nested savepoints, RYOW, counters, runBeforeCommit: false, and Promise rejection', (t) => {
  const tmp = mkdtempSync(join(tmpdir(), 'elmdb-v2-txn-'));
  const dir = join(tmp, 'main');
  const db = Database.open(dir, { schema: baseSchema, durability: 'strict', buildPolicy: 'eager' });
  t.after(() => {
    try { db.close(); } catch { /* already closed */ }
    rmSync(tmp, { recursive: true, force: true });
  });

  assert.equal(db.pendingCommitSeq(), null);
  assert.equal(db.getCounter('NOTE'), 0);

  let hookRuns = 0;
  let hookSawSeq = null;
  let hookSawOuter = false;
  const hid = db.registerBeforeCommit(['Note'], (delta) => {
    hookRuns++;
    hookSawSeq = delta.commitSeq;
    assert.equal(db.pendingCommitSeq(), delta.commitSeq);
    hookSawOuter = db.get('n-outer') !== null && db.get('n-inner-abort') === null;
    db.nextCounter('HOOK');
    return null;
  });

  // Outer + nested savepoints + RYOW + custom Error preservation!
  class CustomRollbackError extends Error {
    constructor(msg) {
      super(msg);
      this.customTag = 'keep-me';
    }
  }

  const ret = db.transact(() => {
    const seq = db.pendingCommitSeq();
    assert.equal(typeof seq, 'number');
    assert.equal(db.nextCounter('NOTE'), 1);
    db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'Outer' }, 'n-outer'); }));

    // Inner savepoint commits
    const innerVal = db.transact(() => {
      assert.equal(db.nextCounter('NOTE'), 2);
      db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'Inner-OK' }, 'n-inner-ok'); }));
      return 'ok-inner';
    });
    assert.equal(innerVal, 'ok-inner');
    assert.equal(hookRuns, 0, 'beforeCommit must not run on inner savepoint release');

    // Inner savepoint throws custom Error -> rolled back without losing outer writes!
    assert.throws(
      () => db.transact(() => {
        assert.equal(db.nextCounter('NOTE'), 3);
        db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'Inner-Abort' }, 'n-inner-abort'); }));
        assert.notEqual(db.get('n-inner-abort'), null);
        throw new CustomRollbackError('abort inner');
      }),
      (err) => err instanceof CustomRollbackError && err.customTag === 'keep-me',
    );

    assert.equal(db.get('n-inner-abort'), null);
    assert.equal(db.getCounter('NOTE'), 2);
    assert.notEqual(db.get('n-outer'), null);
    assert.notEqual(db.get('n-inner-ok'), null);
    return { seq, count: db.query({ source: { kind: 'type', typeName: 'Note' }, terminal: { kind: 'count' } }).count };
  });

  assert.equal(ret.count, 2);
  assert.equal(db.pendingCommitSeq(), null);
  assert.equal(hookRuns, 1);
  assert.equal(hookSawSeq, ret.seq);
  assert.equal(hookSawOuter, true);
  assert.equal(db.getCounter('NOTE'), 2);
  assert.equal(db.getCounter('HOOK'), 1);

  // Async/Promise callback aborts immediately with ASYNC_CALLBACK_FORBIDDEN!
  assert.throws(
    () => db.transact(async () => {
      db.nextCounter('NOTE');
      db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'AsyncBad' }, 'n-async'); }));
    }),
    (err) => err.code === 'ASYNC_CALLBACK_FORBIDDEN',
  );
  assert.equal(db.get('n-async'), null);
  assert.equal(db.getCounter('NOTE'), 2);

  // P0 #4 + P1-1: Returning an object with a throwing `.then` getter OR throwing JS primitives
  // (outer or nested savepoint) MUST roll back cleanly and rethrow the EXACT JS value verbatim!
  assert.throws(
    () => db.transact(() => {
      db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'GetterThrow' }, 'n-getter'); }));
      return {
        get then() {
          throw new Error('getter boom');
        },
      };
    }),
    /getter boom/,
  );
  assert.equal(db.get('n-getter'), null, 'writes must roll back when .then getter throws');

  for (const primitive of ['raw-string-error', 42, false, null, undefined]) {
    let caught = Symbol('not-caught');
    try {
      db.transact(() => {
        db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'PrimRB' }, 'n-prim-rb'); }));
        throw primitive;
      });
    } catch (err) {
      caught = err;
    }
    assert.strictEqual(caught, primitive, `outer transact must rethrow primitive ${String(primitive)} verbatim`);
    assert.equal(db.get('n-prim-rb'), null);
  }

  // Hostile `.then` getter throwing a primitive string also rethrows verbatim & rolls back
  let hostileCaught = null;
  try {
    db.transact(() => {
      db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'HostilePrim' }, 'n-hostile-prim'); }));
      return {
        get then() {
          throw 'hostile-then-string';
        },
      };
    });
  } catch (err) {
    hostileCaught = err;
  }
  assert.strictEqual(hostileCaught, 'hostile-then-string');
  assert.equal(db.get('n-hostile-prim'), null);

  // { runBeforeCommit: false } skips hooks cleanly
  db.transact(() => {
    db.setCounter('IMPORT', 99);
    db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'Catchup' }, 'n-catchup'); }));
  }, { runBeforeCommit: false });
  assert.equal(hookRuns, 1);
  assert.equal(db.getCounter('IMPORT'), 99);
  assert.equal(db.unregisterBeforeCommit(hid), true);

  // Nested savepoint throwing a primitive rolls back inner only and preserves outer write
  db.transact(() => {
    db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'OuterSurvives' }, 'n-outer-prim'); }));
    let nestedCaught = null;
    try {
      db.transact(() => {
        db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'InnerRB' }, 'n-inner-prim'); }));
        throw 'nested-primitive-abort';
      });
    } catch (err) {
      nestedCaught = err;
    }
    assert.strictEqual(nestedCaught, 'nested-primitive-abort');
    assert.equal(db.get('n-inner-prim'), null);
  });
  assert.equal(db.get('n-outer-prim')?.fields.title, 'OuterSurvives');
  assert.equal(db.get('n-inner-prim'), null);

  // P1 #6: Calling db.close() inside db.transact() rejects with INVALID_ARGUMENT without wiping hooks!
  let hookStillAlive = false;
  const hAlive = db.registerBeforeCommit(['Note'], () => {
    hookStillAlive = true;
    return null;
  });
  db.transact(() => {
    assert.throws(() => db.close(), (err) => err.code === 'INVALID_ARGUMENT');
    db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'AfterCloseAttempt' }, 'n-alive'); }));
  });
  assert.equal(hookStillAlive, true, 'failed db.close() inside transact must not wipe registered hooks');
  assert.equal(db.unregisterBeforeCommit(hAlive), true);

  // Dump & load preserves counters
  const dumpFile = join(tmp, 'counters.json');
  db.dump(dumpFile);
  const loadedDir = join(tmp, 'loaded');
  Database.load(dumpFile, loadedDir);
  const loaded = Database.open(loadedDir, { readOnly: true });
  assert.equal(loaded.getCounter('NOTE'), 2);
  assert.equal(loaded.getCounter('HOOK'), 1);
  assert.equal(loaded.getCounter('IMPORT'), 99);
  loaded.close();
});

test('PR #268 Batch 3: unified JSON-to-JS numbers (> u32::MAX, no BigInt) and beforeCommit ASYNC_CALLBACK_FORBIDDEN', async (t) => {
  const tmp = mkdtempSync(join(tmpdir(), 'elmdb-v2-b3-'));
  const dir = join(tmp, 'main');
  const numSchema = {
    types: {
      Metric: {
        fields: {
          bigPos: { type: 'integer' },
          bigNeg: { type: 'integer' },
          maxSafe: { type: 'integer' },
          ts: { type: 'timestamp' },
        },
        indexes: {},
      },
    },
    relations: {
      link: { endpoints: [{ source: 'Metric', target: 'Metric' }], weighted: true },
    },
  };
  const db = Database.open(dir, { schema: numSchema, durability: 'strict', buildPolicy: 'eager' });
  t.after(() => {
    try { db.close(); } catch { /* already closed */ }
    rmSync(tmp, { recursive: true, force: true });
  });

  const nowTs = Date.now();
  const bigPos = 5_000_000_000;
  const bigNeg = -5_000_000_000;
  const maxSafe = Number.MAX_SAFE_INTEGER;
  let hookDelta = null;
  const hid = db.registerBeforeCommit(['Metric'], (delta) => {
    hookDelta = delta;
    return null;
  });

  const batchRes = db.executeWireBatch(buildWireBatch((b) => {
    const a = b.create('Metric', { bigPos, bigNeg, maxSafe, ts: new Date(nowTs) }, 'm-1');
    const c = b.create('Metric', { bigPos, bigNeg, maxSafe, ts: new Date(nowTs) }, 'm-2');
    b.link(a, 'link', c, { weight: 1.5, info: { big: bigPos, ts: new Date(nowTs) } });
  }));
  db.unregisterBeforeCommit(hid);

  // Every single N-API surface emits JS `number` (NEVER `BigInt`) and passes `JSON.stringify` cleanly!
  const row = db.get('m-1');
  assert.equal(typeof row.fields.bigPos, 'number');
  assert.equal(typeof row.fields.bigNeg, 'number');
  assert.equal(typeof row.fields.maxSafe, 'number');
  assert.equal(typeof row.fields.ts, 'number');
  assert.equal(row.fields.bigPos, bigPos);
  assert.equal(row.fields.bigNeg, bigNeg);
  assert.equal(row.fields.maxSafe, maxSafe);
  assert.equal(row.fields.ts, nowTs);
  assert.doesNotThrow(() => JSON.stringify(row));

  const edges = db.getEdges('m-1', 'link', 'out');
  assert.equal(edges.length, 1);
  assert.equal(typeof edges[0].info.big, 'number');
  assert.equal(typeof edges[0].info.ts, 'number');
  assert.equal(edges[0].info.big, bigPos);
  assert.doesNotThrow(() => JSON.stringify(edges));

  const qSpec = { source: { kind: 'type', typeName: 'Metric' }, steps: [], terminal: { kind: 'rows' } };
  const qSync = db.query(qSpec);
  assert.equal(typeof qSync.rows[0].fields.bigPos, 'number');
  assert.equal(typeof qSync.rows[0].fields.ts, 'number');
  assert.doesNotThrow(() => JSON.stringify(qSync));

  const qAsync = await db.queryAsync(qSpec);
  assert.equal(typeof qAsync.rows[0].fields.bigPos, 'number');
  assert.equal(typeof qAsync.rows[0].fields.ts, 'number');
  assert.doesNotThrow(() => JSON.stringify(qAsync));

  const st = db.stats();
  assert.equal(typeof st.mapSizeBytes, 'number');
  assert.equal(typeof st.pageSizeBytes, 'number');
  assert.doesNotThrow(() => JSON.stringify(st));

  const rep = db.openReport();
  assert.equal(typeof rep.observedTxnid, 'number');
  assert.doesNotThrow(() => JSON.stringify(rep));

  assert.equal(typeof batchRes.commitSeq, 'number');
  assert.doesNotThrow(() => JSON.stringify(batchRes));

  assert.equal(typeof hookDelta.commitSeq, 'number');
  assert.doesNotThrow(() => JSON.stringify(hookDelta));

  const dumpFile = join(tmp, 'metrics.json');
  const dumpRep = db.dump(dumpFile);
  assert.equal(typeof dumpRep.entities, 'number');
  assert.doesNotThrow(() => JSON.stringify(dumpRep));

  const reg = db.registerWatch(qSpec, { minGapMs: 0 }, nowTs);
  assert.equal(typeof reg.answer.rows[0].fields.bigPos, 'number');
  assert.doesNotThrow(() => JSON.stringify(reg));
  db.executeWireBatch(buildWireBatch((b) => { b.patch('m-1', { bigPos: 6_000_000_000 }); }));
  const rerun = db.rerunWatch(reg.watchId, qSpec, nowTs + 10, 1);
  assert.equal(typeof rerun.answer.rows[0].fields.bigPos, 'number');
  assert.doesNotThrow(() => JSON.stringify(rerun));
  db.unregisterWatch(reg.watchId);

  // Item 3.6: beforeCommit preserves `ASYNC_CALLBACK_FORBIDDEN` when nested sync API throws it or callback is async.
  const hAsyncNested = db.registerBeforeCommit(['Metric'], () => {
    db.transact(async () => {});
    return null;
  });
  assert.throws(
    () => db.executeWireBatch(buildWireBatch((b) => { b.patch('m-1', { bigPos: 7_000_000_000 }); })),
    (err) => err.code === 'ASYNC_CALLBACK_FORBIDDEN',
  );
  db.unregisterBeforeCommit(hAsyncNested);

  // Hostile `.code` getter on a thrown hook error clears pending exception and still extracts `.message`.
  const hHostileCode = db.registerBeforeCommit(['Metric'], () => {
    const errObj = {
      get code() { throw new Error('hostile code getter'); },
      message: 'keep-this-hook-message',
    };
    throw errObj;
  });
  assert.throws(
    () => db.executeWireBatch(buildWireBatch((b) => { b.patch('m-1', { bigPos: 8_000_000_000 }); })),
    (err) => err.code === 'HOOK_ABORTED' && err.message.includes('keep-this-hook-message'),
  );
  db.unregisterBeforeCommit(hHostileCode);
});
