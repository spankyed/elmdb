// A panic in elmdb is an error JavaScript catches, whose code is PANICKED, and the process lives: a panic that
// unwound out of an export would cross into Node, which aborts.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { Database } = require('../index.js');
const { buildWireBatch } = require('../client.js');
const child = fileURLToPath(new URL('./panics.child.mjs', import.meta.url));
const schema = { types: { Note: { fields: { title: { type: 'string' } } } }, relations: {} };

function seeded(t) {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-panics-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const db = Database.open(dir, { schema, durability: 'unsafe' });
  db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'kept' }, 'Note-kept'); }));
  db.close();
  return dir;
}

const run = (dir, name, asked) =>
  spawnSync(process.execPath, [child, dir, name], { env: { ...process.env, ELMDB_TEST_PANIC: asked }, encoding: 'utf8' });

const exports = [
  'Database.open',
  'Database.openAsync',
  'Database.stats',
  'Database.get',
  'Database.query',
  'Database.queryAsync',
  'Database.executeWireBatch',
  'Database.setSchema',
  'Database.getSchema',
  'Database.check',
  'Database.checkAsync',
  'Database.rebuild',
  'Database.rebuildAsync',
  'Database.flush',
  'Database.flushAsync',
  'Database.backup',
  'Database.backupAsync',
  'Database.close',
];

test('a panic in sync and async exports becomes PANICKED, poisons the handle, and the process lives', (t) => {
  for (const name of exports) {
    const ran = run(seeded(t), name, name);
    assert.equal(ran.status, 0, `${name}: ${ran.signal ?? ''}\n${ran.stdout}\n${ran.stderr}`);
    assert.equal(ran.stdout, `${name} panicked, and the process lives\n`);
    assert.match(ran.stderr, /panicked at .*panics\.rs/, name);
  }
});

test('an export panics only where it was asked to', (t) => {
  for (const asked of ['Database.get', '1', '']) {
    const ran = run(seeded(t), 'Database.stats', asked);
    assert.notEqual(ran.status, 0, asked);
    assert.match(ran.stderr, /Database\.stats: undefined/, asked);
  }
});
