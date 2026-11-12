// A panic in elmdb is an error JavaScript catches, whose code is PANICKED, and the process lives: a panic that
// unwound out of an export would cross into Node, which aborts. Each export is called in a child started with
// ELMDB_TEST_PANIC naming it, which has that export panic the first time it is called
// (crates/elmdb-node/src/panics.rs).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const { Database, Unit } = createRequire(import.meta.url)('../index.js');
const child = fileURLToPath(new URL('./panics.child.mjs', import.meta.url));
const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });
const schema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  types: { Note: { fields: { title: { type: 'string' } } } },
};

/** A database holding one note, for a child to open */
function seeded(t) {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-panics-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const db = Database.open(dir, { schema, syncs: false });
  db.write(packr.pack({ ops: [{ op: 'create', type: 'Note', id: 'Note-kept', fields: { title: 'kept' } }], now: 1000 }));
  db.close();
  return dir;
}

const run = (dir, name, asked) =>
  spawnSync(process.execPath, [child, dir, name], { env: { ...process.env, ELMDB_TEST_PANIC: asked }, encoding: 'utf8' });

// Every member the two classes have, the factory, and the work two of them do on a thread of the pool
const members = (prototype) => Object.getOwnPropertyNames(prototype).filter((name) => name !== 'constructor');
const exports = [
  'Database.open',
  ...members(Database.prototype).map((name) => `Database.${name}`),
  ...members(Unit.prototype).map((name) => `Unit.${name}`),
  'Database.read, on a thread of the pool,',
  'Database.durable, on a thread of the pool,',
];

test('a panic in each export is an error with a code of its own, and the process lives', (t) => {
  assert.ok(exports.length > 50, `${exports.length} exports`);
  for (const name of exports) {
    const ran = run(seeded(t), name, name);
    assert.equal(ran.status, 0, `${name}: ${ran.signal ?? ''}\n${ran.stdout}\n${ran.stderr}`);
    assert.equal(ran.stdout, `${name} panicked, and the process lives\n`);
    // The panic is still reported where panics are, with the place it was raised
    assert.match(ran.stderr, /panicked at .*panics\.rs/, name);
  }
});

test('an export panics only where it was asked to', (t) => {
  // The child expects a panic of the export it is given, and here another was asked for one, or none
  for (const asked of ['Database.stats', '1', '']) {
    const ran = run(seeded(t), 'Database.sequence', asked);
    assert.notEqual(ran.status, 0, asked);
    assert.match(ran.stderr, /Database\.sequence: undefined/, asked);
  }
});
