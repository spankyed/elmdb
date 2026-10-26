// One elmdb binary per process (src/process.rs): two copies of LMDB in one process share no locks, so an open from a
// second binary is refused, and a child process, which inherits its parent's environment, is not held to its parent's.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { copyFileSync, mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = fileURLToPath(new URL('..', import.meta.url));
const binary = join(here, readdirSync(here).find((file) => file.endsWith('.node')));
const loader = join(here, 'index.js');

/** Runs `source` in a fresh Node process, answering what it prints */
const run = (source) => execFileSync(process.execPath, ['-e', source], { encoding: 'utf8' }).trim();

test('an open from a second elmdb binary in the process is refused, naming both', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-claim-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const copy = join(dir, 'copy.node');
  copyFileSync(binary, copy);
  const printed = run(`
    const first = require(${JSON.stringify(binary)});
    const second = require(${JSON.stringify(copy)});
    first.Database.open(${JSON.stringify(join(dir, 'a'))}, { syncs: false });
    try {
      second.Database.open(${JSON.stringify(join(dir, 'b'))}, { syncs: false });
      console.log('opened');
    } catch (error) {
      console.log(JSON.stringify({ code: error.code, names: error.message.includes(${JSON.stringify(copy)}) }));
    }
  `);
  assert.deepEqual(JSON.parse(printed), { code: 'ALREADY_LOADED', names: true });
});

test('a child process opens with a binary of its own, whatever its parent claimed', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-claim-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const copy = join(dir, 'copy.node');
  copyFileSync(binary, copy);
  const printed = run(`
    require(${JSON.stringify(loader)}).Database.open(${JSON.stringify(join(dir, 'a'))}, { syncs: false });
    const { execFileSync } = require('node:child_process');
    process.stdout.write(execFileSync(process.execPath, ['-e', ${JSON.stringify(`
      require(${JSON.stringify(copy)}).Database.open(${JSON.stringify(join(dir, 'b'))}, { syncs: false });
      console.log('opened');
    `)}], { encoding: 'utf8' }));
  `);
  assert.equal(printed, 'opened');
});
