// The power-loss-safe mode: `syncEachCommit` syncs every commit as it is made, so no group commit runs and a commit that
// returned is on disk. The file-level replay of the loss is the engine's (`engine/tests/power_loss.rs`); this holds the
// option through the binding: the mode runs no group-commit sync at all, the default runs one for a durable write, and what
// was committed is there after the database is closed and opened again.
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
const batch = (i) => packr.pack({ ops: [{ op: 'create', type: 'Note', id: `Note-${i}`, fields: { title: `note ${i}` } }] });

function dir(t) {
  const path = mkdtempSync(join(tmpdir(), 'elmdb-durable-'));
  t.after(() => rmSync(path, { recursive: true, force: true }));
  return path;
}

test('a database opened to sync each commit runs no group-commit sync, and keeps what it wrote', async (t) => {
  const path = dir(t);
  const db = Database.open(path, { schema, syncEachCommit: true });
  for (let i = 0; i < 20; i++) db.writeSync(batch(i), i % 2 === 0);
  await db.durable();
  assert.equal(db.syncs, 0, 'each commit was synced as it was made, by LMDB, and no group commit ran');
  db.close();
  const again = Database.open(path, { schema });
  const { n } = packr.unpack(again.readSync(packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'count' } } }] })))[0];
  again.close();
  assert.equal(n, 20);
});

test('a database opened without the option group-commits: a durable write waits for a sync it runs', (t) => {
  const path = dir(t);
  const db = Database.open(path, { schema });
  db.writeSync(batch(1), true);
  assert.ok(db.syncs > 0, 'the durable write waited for a group-commit sync');
  db.close();
});
