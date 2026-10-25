import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const { Database } = createRequire(import.meta.url)('../index.js');
const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });

test('reads tracked under a token cross the binding, and a commit names the token it wrote into', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  const db = Database.open(dir, { syncs: false, schema: {
    $schema: 'https://elmdb.dev/schema/1.json',
    types: { Task: { fields: { title: { type: 'any', optional: true } } }, Note: { fields: {} } },
  } });
  t.after(() => { db.close(); rmSync(dir, { recursive: true, force: true }); });
  const write = (ops) => packr.unpack(db.write(packr.pack({ ops, now: 1000 })));
  const tasks = { read: 'query', plan: { start: { from: 'types', types: ['Task'] }, terminal: { answer: 'ids' } } };

  db.readSync(packr.pack({ reads: [tasks], track: 4 }));
  assert.deepEqual(db.trackStats().owners, 1);
  const before = db.sequence;
  const created = write([{ op: 'create', type: 'Task', id: 'Task-1' }]);
  assert.deepEqual(created.changes.touched, [4]);
  assert.equal(created.sequence, before + 1);
  assert.equal(db.sequence, before + 1);
  assert.deepEqual(write([{ op: 'create', type: 'Note', id: 'Note-1' }]).changes.touched, []);

  const unit = db.begin();
  unit.write(packr.pack({ ops: [{ op: 'create', type: 'Task', id: 'Task-2' }], now: 1000 }));
  const committed = packr.unpack(unit.commit());
  assert.deepEqual([committed.touched, committed.sequence], [[4], before + 3]);

  db.untrack(4);
  assert.deepEqual(db.trackStats(), { owners: 0, keys: 0, ranges: 0 });
  assert.deepEqual(write([{ op: 'create', type: 'Task', id: 'Task-3' }]).changes.touched, []);
});
