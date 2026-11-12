// A panic in elmdb is an error JavaScript catches, whose code is PANICKED, and the process lives: a panic that
// unwound out of an export would cross into Node, which aborts. The child is started with ELMDB_TEST_PANICS set,
// which has each export panic the first time it is called (crates/elmdb-node/src/panics.rs).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const child = fileURLToPath(new URL('./panics.child.mjs', import.meta.url));

test('a panic in each export is an error with a code of its own, and the process lives', () => {
  const ran = spawnSync(process.execPath, [child], { env: { ...process.env, ELMDB_TEST_PANICS: '1' }, encoding: 'utf8' });
  assert.equal(ran.status, 0, `${ran.signal ?? ''}\n${ran.stdout}\n${ran.stderr}`);
  assert.match(ran.stdout, /every export panicked once, and the process lives\n$/);
  // Each panic is still reported where panics are, with the place it was raised
  assert.match(ran.stderr, /panicked at .*panics\.rs/);
});

test('without the variable no export panics', () => {
  const { Database } = createRequire(import.meta.url)('../index.js');
  assert.equal(typeof Database.open, 'function');
  const ran = spawnSync(process.execPath, [child], { env: { ...process.env, ELMDB_TEST_PANICS: undefined }, encoding: 'utf8' });
  assert.notEqual(ran.status, 0, 'the child expects a panic the first time');
  assert.match(ran.stderr, /Database\.open: undefined/);
});
