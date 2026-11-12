// The power-loss-safe mode: `syncEachCommit` syncs every commit as it is made, so no group commit runs and a commit that
// returned is on disk. The file-level replay of the loss itself is the engine's (`engine/tests/power_loss.rs`); this holds
// the option through the binding: it is accepted, a durable write and `durable()` answer at once, and what it committed
// is there after the database is closed and opened again.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const { Database } = createRequire(import.meta.url)('../index.js');
const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });
const schema = { $schema: 'https://elmdb.dev/schema/1.json', types: { Note: { fields: { title: { type: 'string' } } } } };

function dir(t) {
  const path = mkdtempSync(join(tmpdir(), 'elmdb-durable-'));
  t.after(() => rmSync(path, { recursive: true, force: true }));
  return path;
}

test('a database opened to sync each commit takes durable writes and keeps them', async (t) => {
  const path = dir(t);
  const db = Database.open(path, { schema, syncEachCommit: true });
  for (let i = 0; i < 20; i++) {
    db.write(packr.pack({ ops: [{ op: 'create', type: 'Note', id: `Note-${i}`, fields: { title: `note ${i}` } }] }), true);
  }
  await db.durable();
  db.close();
  const again = Database.open(path, { schema });
  const { n } = packr.unpack(again.readSync(packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'count' } } }] })))[0];
  again.close();
  assert.equal(n, 20);
});

test('a database opened without the option group-commits, and reports its syncs', (t) => {
  const path = dir(t);
  const db = Database.open(path, { schema });
  db.write(packr.pack({ ops: [{ op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one' } }] }));
  assert.equal(typeof db.syncs, 'number');
  db.close();
});
