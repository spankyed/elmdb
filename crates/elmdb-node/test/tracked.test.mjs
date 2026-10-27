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

  // A number that isn't a token's is refused, never wrapped onto one
  for (const bad of [4 + 2 ** 32, -1, 4.5, Number.NaN]) {
    assert.throws(() => db.untrack(bad), { code: 'BATCH_INVALID' }, String(bad));
  }
  assert.equal(db.trackStats().owners, 1);
  db.untrack(4);
  assert.deepEqual(db.trackStats(), { owners: 0, keys: 0, ranges: 0 });
  assert.deepEqual(write([{ op: 'create', type: 'Task', id: 'Task-3' }]).changes.touched, []);
});

test('pooled reads keep query results coherent while an index changes fields', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-schema-pool-'));
  const schema = (field) => ({
    $schema: 'https://elmdb.dev/schema/1.json',
    types: { Task: { fields: { a: { type: 'string' }, b: { type: 'string' } },
      indexes: { lookup: { kind: 'value', fields: [field] } } } },
  });
  const db = Database.open(dir, { syncs: false, schema: schema('a') });
  t.after(() => { db.close(); rmSync(dir, { recursive: true, force: true }); });
  db.write(packr.pack({ ops: Array.from({ length: 1000 }, (_, i) => ({
    op: 'create', type: 'Task', id: `Task-${i}`,
    fields: { a: i === 0 ? 'match' : 'other', b: i === 1 ? 'match' : 'other' },
  })) }));
  const query = { read: 'query', plan: { start: { from: 'types', types: ['Task'] },
    steps: [{ step: 'where', field: 'a', value: 'match' }], terminal: { answer: 'ids' } } };
  for (let round = 0; round < 8; ++round) {
    const pending = Array.from({ length: 8 }, (_, i) => db.read(packr.pack({
      reads: [query], ...(i % 2 ? { track: round } : {}),
    })));
    db.setSchema(schema(round % 2 ? 'a' : 'b'));
    db.waitForIndexBuilds();
    for (const bytes of await Promise.all(pending)) {
      assert.deepEqual(packr.unpack(bytes)[0].ids, ['Task-0']);
    }
    db.untrack(round);
  }
});
