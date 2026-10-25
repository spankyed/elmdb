import { test } from 'node:test';
import assert from 'node:assert/strict';
import { copyFileSync, mkdtempSync, existsSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const { Database } = require('../index.js');
const here = dirname(fileURLToPath(import.meta.url));

/** A fresh directory, removed when the test ends */
function tempDir(t) {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

const noteSchema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  types: { Note: { shortCode: { prefix: 'NTE' }, fields: { title: { type: 'string' } } } },
};

test('opens a database, creating its files, and closes it', (t) => {
  const dir = join(tempDir(t), 'db');
  const db = Database.open(dir, { syncs: false });
  assert.equal(db.isOpen, true);
  assert.equal(db.readOnly, false);
  assert.equal(db.schema, null);
  assert.ok(existsSync(join(dir, 'data.mdb')));
  db.close();
  assert.equal(db.isOpen, false);
  db.close();
  assert.throws(() => db.syncs, { code: 'CLOSED' });
});

test('a second open in one process throws ALREADY_OPEN', (t) => {
  const dir = tempDir(t);
  const db = Database.open(dir, { syncs: false });
  assert.throws(() => Database.open(dir), { code: 'ALREADY_OPEN' });
  db.close();
  Database.open(dir, { syncs: false }).close();
});

test('a read-only open of an empty directory throws NO_DATABASE', (t) => {
  const dir = tempDir(t);
  assert.throws(() => Database.open(dir, { readOnly: true }), { code: 'NO_DATABASE', message: /No database at/ });
});

test('stores a schema, reports what opening did, and refuses an invalid one listing every problem', (t) => {
  const dir = tempDir(t);
  const db = Database.open(dir, { schema: noteSchema, syncs: false });
  assert.deepEqual(db.openReport, { schemaChanges: [{ path: 'types.Note', change: 'added', effect: 'applied' }] });
  assert.equal(db.schema.types.Note.additionalFields, 'refuse', 'the canonical form, defaults written');
  db.close();
  const bad = structuredClone(noteSchema);
  bad.types.Note.fields.title.type = 'text';
  bad.types.Note.extra = true;
  assert.throws(() => Database.open(dir, { schema: bad }), (error) => {
    assert.equal(error.code, 'SCHEMA_INVALID');
    assert.match(error.message, /2 problems/);
    assert.match(error.message, /types\.Note\.extra: unknown key/);
    assert.match(error.message, /types\.Note\.fields\.title\.type: "text" isn't a field type/);
    return true;
  });
});

test('durable() resolves once what was committed is on disk', async (t) => {
  const dir = tempDir(t);
  const db = Database.open(dir, { schema: noteSchema });
  // Opening stored the schema with a sync of its own, so nothing is owed
  const before = db.syncs;
  await db.durable();
  assert.equal(db.syncs, before);
  db.close();
});

test('another elmdb binary in the process is refused at open, naming both files', (t) => {
  const dir = tempDir(t);
  // The first open claims the process for the binary index.js loaded
  Database.open(join(dir, 'first'), { syncs: false }).close();
  const binary = readdirSync(join(here, '..')).find((f) => f.endsWith('.node'));
  const copy = join(dir, 'copy', binary);
  rmSync(dirname(copy), { recursive: true, force: true });
  require('node:fs').mkdirSync(dirname(copy));
  copyFileSync(join(here, '..', binary), copy);
  const second = require(copy);
  assert.throws(() => second.Database.open(join(dir, 'second'), { syncs: false }), (error) => {
    assert.equal(error.code, 'ALREADY_LOADED');
    assert.ok(error.message.includes(copy), error.message);
    assert.ok(error.message.includes(binary), error.message);
    return true;
  });
});
