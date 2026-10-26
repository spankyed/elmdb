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

test('setSchema changes an open database\'s schema, and refuses while a unit is open', (t) => {
  const dir = tempDir(t);
  const db = Database.open(dir, { schema: noteSchema, syncs: false });
  const wider = structuredClone(noteSchema);
  wider.types.Note.fields.body = { type: 'string', optional: true };
  assert.deepEqual(db.setSchema(wider).map((c) => c.path), ['types.Note.fields.body']);
  assert.ok('body' in db.schema.types.Note.fields);
  assert.deepEqual(db.setSchema(wider), [], 'the same schema changes nothing');
  const unit = db.begin();
  assert.throws(() => db.setSchema(noteSchema), (error) => error.code === 'UNIT_OPEN');
  unit.abort();
  assert.throws(() => db.setSchema({ ...noteSchema, types: { note: {} } }), (error) => error.code === 'SCHEMA_INVALID');
  db.close();
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

test('rebuild() takes a type and an index, or neither', (t) => {
  const db = Database.open(tempDir(t), { schema: noteSchema, syncs: false });
  t.after(() => db.close());
  for (const args of [['Note'], [null, 'byTitle'], [undefined, 'byTitle']]) {
    assert.throws(() => db.rebuild(...args), { code: 'BATCH_INVALID' }, JSON.stringify(args));
  }
  db.rebuild();
});

test('another elmdb binary in the process is refused at open, naming both files', (t) => {
  const dir = tempDir(t);
  // The first open claims the process for the binary index.js loaded
  Database.open(join(dir, 'first'), { syncs: false }).close();
  const binary = readdirSync(join(here, '..')).find((f) => f.endsWith('.node'));
  // Windows holds a loaded binary's file open until the process exits, so the copy can't be removed after the test
  const copyDir = mkdtempSync(join(tmpdir(), 'elmdb-binary-'));
  t.after(() => {
    try {
      rmSync(copyDir, { recursive: true, force: true });
    } catch (error) {
      if (process.platform !== 'win32' || error.code !== 'EPERM') throw error;
    }
  });
  const copy = join(copyDir, binary);
  copyFileSync(join(here, '..', binary), copy);
  const second = require(copy);
  assert.throws(() => second.Database.open(join(dir, 'second'), { syncs: false }), (error) => {
    assert.equal(error.code, 'ALREADY_LOADED');
    assert.ok(error.message.includes(copy), error.message);
    assert.ok(error.message.includes(binary), error.message);
    return true;
  });
});

test('an open refuses a sync or map option that is not a whole number in range, rather than casting it', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  for (const options of [{ syncIdleMs: -1 }, { syncIntervalMs: 1.5 }, { syncIdleMs: Number.NaN }, { mapSizeBytes: 0 }, { mapSizeBytes: Infinity }]) {
    assert.throws(() => Database.open(dir, { syncs: false, ...options }), { code: 'VALUE_INVALID' }, JSON.stringify(options));
  }
  Database.open(dir, { syncIdleMs: 0, syncIntervalMs: 5000 }).close();
});
