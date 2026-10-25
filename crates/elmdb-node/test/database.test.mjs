import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';

const { Database } = createRequire(import.meta.url)('../index.js');

/** A fresh directory, removed when the test ends */
function tempDir(t) {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test('opens a database, creating its files, and closes it', (t) => {
  const dir = join(tempDir(t), 'db');
  const db = Database.open(dir);
  assert.equal(db.isOpen, true);
  assert.equal(db.readOnly, false);
  assert.ok(existsSync(join(dir, 'data.mdb')));
  db.close();
  assert.equal(db.isOpen, false);
  db.close();
});

test('a second open in one process throws ALREADY_OPEN', (t) => {
  const dir = tempDir(t);
  const db = Database.open(dir);
  assert.throws(() => Database.open(dir), { code: 'ALREADY_OPEN' });
  db.close();
  Database.open(dir).close();
});

test('a read-only open of an empty directory throws NO_DATABASE', (t) => {
  const dir = tempDir(t);
  assert.throws(() => Database.open(dir, { readOnly: true }), { code: 'NO_DATABASE', message: /No database at/ });
});
