// One elmdb binary per process (src/process.rs): two copies of LMDB in one process share no locks, so an open from a
// second binary is refused. A child process has an independent claim.
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
const run = (source) => execFileSync(process.execPath, ['-e', source], { encoding: 'utf8', timeout: 15000 }).trim();

test('an open from a second elmdb binary in the process is refused, naming the rejected copy', (t) => {
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

test('claiming a process does not mutate its environment', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-claim-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  assert.equal(run(`
    const before = process.env.ELMDB_BINDING;
    const db = require(${JSON.stringify(binary)}).Database.open(${JSON.stringify(join(dir, 'a'))}, { syncs: false });
    db.close();
    console.log(process.env.ELMDB_BINDING === before ? 'unchanged' : 'mutated');
  `), 'unchanged');
});

test('concurrent first opens in different workers admit only one physical binary', { timeout: 20000 }, async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-claim-race-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const copy = join(dir, 'copy.node');
  copyFileSync(binary, copy);
  const { promisify } = await import('node:util');
  const { execFile } = await import('node:child_process');
  const { stdout } = await promisify(execFile)(process.execPath, ['-e', `
    const { Worker } = require('node:worker_threads');
    const barrier = new SharedArrayBuffer(4);
    const gate = new Int32Array(barrier);
    const source = \`
      const { workerData: d, parentPort } = require('node:worker_threads');
      const binding = require(d.binary);
      const gate = new Int32Array(d.barrier);
      parentPort.postMessage('ready');
      Atomics.wait(gate, 0, 0);
      try {
        const db = binding.Database.open(d.path, { syncs: false });
        db.close();
        parentPort.postMessage('opened');
      } catch (e) { parentPort.postMessage(e.code); }
    \`;
    let ready = 0;
    const results = [];
    for (const [i, binary] of ${JSON.stringify([binary, copy])}.entries()) {
      const worker = new Worker(source, { eval: true, env: { ...process.env, TMPDIR: '/elmdb-worker-temp-' + i }, workerData: { binary, barrier, path: ${JSON.stringify(dir)} + '/db' + i } });
      worker.on('error', e => { console.error(e); process.exitCode = 1; });
      worker.on('message', message => {
        if (message === 'ready') {
          if (++ready === 2) { Atomics.store(gate, 0, 1); Atomics.notify(gate, 0); }
        } else {
          results.push(message);
          if (results.length === 2) console.log(JSON.stringify(results.sort()));
        }
      });
    }
  `], { timeout: 15000 });
  assert.deepEqual(JSON.parse(stdout.trim()), ['ALREADY_LOADED', 'opened']);
});

test('a claim outlives its first worker and database closes', { timeout: 20000 }, async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-claim-worker-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const copy = join(dir, 'copy.node');
  copyFileSync(binary, copy);
  const { promisify } = await import('node:util');
  const { execFile } = await import('node:child_process');
  const { stdout } = await promisify(execFile)(process.execPath, ['-e', `
    const { Worker } = require('node:worker_threads');
    const w = new Worker(${JSON.stringify(`
      const db = require(${JSON.stringify(binary)}).Database.open(${JSON.stringify(join(dir, 'a'))}, { syncs: false });
      db.close();
    `)}, { eval: true });
    w.on('error', e => { console.error(e); process.exitCode = 1; });
    w.on('exit', code => {
      if (code !== 0) return;
      const first = require(${JSON.stringify(binary)});
      first.Database.open(${JSON.stringify(join(dir, 'b'))}, { syncs: false }).close();
      const second = require(${JSON.stringify(copy)});
      const codes = [];
      for (let i = 0; i < 2; ++i) {
        try { second.Database.open(${JSON.stringify(join(dir, 'c'))}, { syncs: false }).close(); codes.push('opened'); }
        catch (e) { codes.push(e.code); }
      }
      console.log(JSON.stringify(codes));
    });
  `], { timeout: 15000 });
  assert.deepEqual(JSON.parse(stdout.trim()), ['ALREADY_LOADED', 'ALREADY_LOADED']);
});

if (process.platform !== 'win32') {
  test('an unusable process claim refuses before creating a database', (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'elmdb-claim-io-'));
    t.after(() => rmSync(dir, { recursive: true, force: true }));
    const printed = run(`
      const fs = require('node:fs');
      const claim = '/tmp/elmdb-binding-' + process.getuid() + '-' + process.pid + '.lock';
      fs.mkdirSync(claim);
      try {
        let code;
        try { require(${JSON.stringify(binary)}).Database.open(${JSON.stringify(join(dir, 'a'))}, { syncs: false }); }
        catch (e) { code = e.code; }
        console.log(JSON.stringify({ code, created: fs.existsSync(${JSON.stringify(join(dir, 'a'))}) }));
      } finally { fs.rmdirSync(claim); }
    `);
    assert.deepEqual(JSON.parse(printed), { code: 'IO', created: false });
  });
}
