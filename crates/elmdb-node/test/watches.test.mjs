// Sans-I/O live query watch hub over N-API: registerWatch, tickWatches, nextDueWatch, rerunWatch, cross-process catch-up.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { Database } = require('../index.js');
const { buildWireBatch } = require('../client.js');

const schema = {
  types: {
    Task: {
      fields: {
        title: { type: 'string' },
        done: { type: 'boolean' },
      },
      indexes: {
        byDone: { kind: 'value', fields: ['done'] },
      },
    },
    Other: {
      fields: { name: { type: 'string' } },
    },
  },
  relations: {},
};

test('live watches invalidate selectively, dedup unchanged results, and catch up cross-handle commits', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-v2-watch-'));
  let db1 = null;
  let db2 = null;
  t.after(() => {
    try { db2?.close(); } finally {
      try { db1?.close(); } finally {
        rmSync(dir, { recursive: true, force: true });
      }
    }
  });
  db1 = Database.open(dir, { schema, durability: 'strict', buildPolicy: 'eager' });

  const openTasksSpec = {
    source: { kind: 'type', typeName: 'Task' },
    steps: [{ kind: 'filter', expr: { op: 'eq', field: 'done', value: false } }],
    terminal: { kind: 'ids' },
  };

  const reg = db1.registerWatch(openTasksSpec, { mode: 'declarative', minGapMs: 10 }, 1000);
  assert.deepEqual(reg.answer.ids, []);
  assert.equal(db1.watchesSettled(), true);

  // Unrelated type write must NOT dirty the Task watch!
  db1.executeWireBatch(buildWireBatch((b) => { b.create('Other', { name: 'unrelated' }, 'o1'); }));
  assert.equal(db1.watchesSettled(), true);

  // Matching Task write dirties the watch immediately!
  db1.executeWireBatch(buildWireBatch((b) => { b.create('Task', { title: 'Ship v2', done: false }, 't1'); }));
  assert.equal(db1.watchesSettled(), false);

  const future1 = Date.now() + 60_000;
  const nd = db1.nextDueWatch(future1, 0);
  assert.equal(nd.watchId, reg.watchId);
  const rerun = db1.rerunWatch(reg.watchId, openTasksSpec, future1, 1);
  assert.equal(rerun.changed, true);
  assert.deepEqual(rerun.answer.ids, ['t1']);
  assert.equal(db1.watchesSettled(), true);

  // Cross-handle (simulating cross-process) write caught up selectively via persisted commit_log!
  db2 = Database.open(dir, { durability: 'strict' });
  db2.executeWireBatch(buildWireBatch((b) => { b.create('Task', { title: 'Cross-proc', done: false }, 't2'); }));

  const future2 = future1 + 60_000;
  const dirty = db1.catchUpCrossProcess(future2);
  assert.deepEqual(dirty, [reg.watchId]);

  const tick = db1.tickWatches(future2 + 1000);
  assert.deepEqual(tick.dueIds, [reg.watchId]);
  const rerun2 = db1.rerunWatch(reg.watchId, openTasksSpec, future2 + 1000, 1);
  assert.equal(rerun2.changed, true);
  assert.equal(rerun2.answer.ids.length, 2);
  assert.equal(db1.watchesSettled(), true);

  assert.equal(db1.unregisterWatch(reg.watchId), true);

  // P0-7: Failing rerunWatch aborts running=false instead of wedging watchesSettled() forever!
  const reg2 = db1.registerWatch(openTasksSpec, { mode: 'declarative', minGapMs: 0 }, 1000);
  db1.executeWireBatch(buildWireBatch((b) => { b.create('Task', { title: 'Trigger', done: false }, 't3'); }));
  const nd2 = db1.nextDueWatch(future2 + 2000, 0);
  assert.equal(nd2.watchId, reg2.watchId);
  assert.throws(
    () => db1.rerunWatch(reg2.watchId, { source: { kind: 'type', typeName: 'MissingType' }, steps: [], terminal: { kind: 'ids' } }, future2 + 2000, 1),
    (err) => err.code === 'UNKNOWN_TYPE',
  );
  // Watch was cleanly aborted & requeued (not wedged in running=true), so a valid rerun settles it!
  const recovered = db1.rerunWatch(reg2.watchId, openTasksSpec, future2 + 3000, 1);
  assert.equal(recovered.answer.ids.length, 3);
  assert.equal(db1.watchesSettled(), true);

  db1.stopWatches();
});
