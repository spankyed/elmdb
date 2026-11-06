// One elmdb binary per process (src/process.rs): two copies of LMDB in one process share no locks, so an open from a
// second binary is refused. A child process has an independent claim.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { copyFileSync, mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const here = fileURLToPath(new URL('..', import.meta.url));
const binary = join(here, readdirSync(here).find((file) => file.endsWith('.node')));
const loader = join(here, 'index.js');
const require = createRequire(import.meta.url);

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
      console.log(JSON.stringify({ code: error.code, names: error.message.includes(${JSON.stringify(copy)}),
        explainsCollision: process.platform !== 'darwin' || error.message.includes('unrelated component') }));
    }
  `);
  assert.deepEqual(JSON.parse(printed), { code: 'ALREADY_LOADED', names: true, explainsCollision: true });
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

test('changing temporary-directory settings cannot create another process claim', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-claim-env-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const copy = join(dir, 'copy.node');
  copyFileSync(binary, copy);
  const printed = run(`
    const first = require(${JSON.stringify(binary)});
    first.Database.open(${JSON.stringify(join(dir, 'a'))}, { syncs: false }).close();
    for (const key of ['TMPDIR', 'TMP', 'TEMP', 'SystemTemp', 'HOME', 'USERPROFILE']) process.env[key] = ${JSON.stringify(dir)};
    let code;
    try { require(${JSON.stringify(copy)}).Database.open(${JSON.stringify(join(dir, 'b'))}, { syncs: false }).close(); code = 'opened'; }
    catch (error) { code = error.code; }
    first.Database.open(${JSON.stringify(join(dir, 'a'))}, { syncs: false }).close();
    console.log(code);
  `);
  assert.equal(printed, 'ALREADY_LOADED');
});

test('claim acquisition needs no usable home or temporary directory', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-claim-no-home-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  assert.equal(run(`
    for (const key of ['HOME', 'USERPROFILE', 'TMPDIR', 'TMP', 'TEMP', 'XDG_RUNTIME_DIR'])
      process.env[key] = '/nonexistent/elmdb-test';
    require(${JSON.stringify(binary)}).Database.open(${JSON.stringify(join(dir, 'db'))}, { syncs: false }).close();
    console.log('opened');
  `), 'opened');
});

if (process.platform !== 'win32') {
  test('claiming the process creates no home-directory coordination files', (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'elmdb-claim-no-files-'));
    t.after(() => rmSync(dir, { recursive: true, force: true }));
    assert.equal(run(`
      const fs = require('node:fs');
      const root = require('node:path').join(require('node:os').userInfo().homedir, '.elmdb', 'process-claims');
      const prefix = 'elmdb-binding-' + process.geteuid() + '-' + process.pid + '-';
      const snapshot = () => fs.existsSync(root) ? fs.readdirSync(root).filter(name => name.startsWith(prefix)).sort() : [];
      const before = snapshot();
      require(${JSON.stringify(binary)}).Database.open(${JSON.stringify(join(dir, 'db'))}, { syncs: false }).close();
      console.log(JSON.stringify(before) === JSON.stringify(snapshot()) ? 'unchanged' : 'created files');
    `), 'unchanged');
  });
}

test('a commit from another process wakes a listener, and a killed writer leaves it listening', async (t) => {
  const { Database } = require(loader);
  const schema = { $schema: 'https://elmdb.dev/schema/1.json', types: { Task: { fields: {} } } };
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-wake-'));
  const db = Database.open(dir, { syncs: false, schema });
  t.after(() => {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  });
  const wakes = [];
  let woken = () => {};
  db.hearCommitsElsewhere((sequence) => {
    wakes.push(sequence);
    woken();
  });
  assert.throws(() => db.hearCommitsElsewhere(() => {}), { code: 'ALREADY_LISTENING' });

  /** Writes a row from a process of its own, which then does `then`, and waits for the wake */
  const writeElsewhere = async (id, then) => {
    const source = `
      const { Database } = require(${JSON.stringify(loader)});
      const { Packr } = require(${JSON.stringify(require.resolve('msgpackr'))});
      const other = Database.open(${JSON.stringify(dir)}, { syncs: false, schema: ${JSON.stringify(schema)} });
      other.write(new Packr({ useRecords: false }).pack({ ops: [{ op: 'create', type: 'Task', id: ${JSON.stringify(id)} }], now: 1000 }));
      ${then}
    `;
    const heard = new Promise((resolve) => (woken = resolve));
    const child = spawn(process.execPath, ['-e', source], { stdio: ['ignore', 'ignore', 'pipe'] });
    let errors = '';
    child.stderr.on('data', (chunk) => (errors += chunk));
    const refused = new Promise((_, reject) => {
      child.on('exit', (code) => {
        if (code) reject(new Error(errors));
      });
    });
    await Promise.race([heard, refused]);
  };

  await writeElsewhere('Task-a', 'other.close();');
  const first = wakes.at(-1);
  assert.ok(first > 0);
  assert.equal(db.committedElsewhere(), true, 'the other process committed');
  assert.equal(db.committedElsewhere(), false, 'and nothing since this asked');
  await writeElsewhere('Task-b', 'process.kill(process.pid, "SIGKILL");');
  assert.ok(wakes.at(-1) > first, 'heard the commit of a process that was then killed');
  await writeElsewhere('Task-c', 'other.close();');
});

test('a reset from another process wakes a listener, refuses commits here until adopted, and leaves reads', async (t) => {
  const { Database } = require(loader);
  const { Packr } = require('msgpackr');
  const packr = new Packr({ useRecords: false });
  const schema = { $schema: 'https://elmdb.dev/schema/1.json', types: { Task: { fields: {} } } };
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-epoch-'));
  const db = Database.open(dir, { syncs: false, schema });
  t.after(() => {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  });
  const write = (id) => db.write(packr.pack({ ops: [{ op: 'create', type: 'Task', id }], now: 1000 }));
  write('Task-a');
  const woken = new Promise((resolve) => db.hearCommitsElsewhere(resolve));

  run(`
    const { Database } = require(${JSON.stringify(loader)});
    const other = Database.open(${JSON.stringify(dir)}, { syncs: false });
    other.reset();
    other.close();
  `);
  await woken;
  const epoch = db.replacedElsewhere();
  assert.equal(typeof epoch, 'number');
  assert.throws(() => write('Task-b'), { code: 'DATA_REPLACED' });

  assert.equal(db.adoptReplacement(epoch + 1), false, 'not the stored one');
  assert.equal(db.adoptReplacement(epoch), true);
  assert.equal(db.replacedElsewhere(), null);
  write('Task-c');
});

test('a reader reads what another process changed since its position, and a unit says what it commits as', (t) => {
  const { Database } = require(loader);
  const { Packr } = require('msgpackr');
  const packr = new Packr({ useRecords: false });
  const schema = { $schema: 'https://elmdb.dev/schema/1.json', types: { Task: { fields: {}, changeOrder: true }, Note: { fields: {} } } };
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-changed-'));
  const db = Database.open(dir, { syncs: false, schema });
  t.after(() => {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  });
  const create = (id) => db.write(packr.pack({ ops: [{ op: 'create', type: 'Task', id }], now: 1000 }));
  create('Task-mine');
  const read = (from) => packr.unpack(db.changedSince(['Task'], packr.pack(from), 100));
  const cursor = read({ sequence: 0 }).next;

  run(`
    const { Database } = require(${JSON.stringify(loader)});
    const { Packr } = require(${JSON.stringify(require.resolve('msgpackr'))});
    const other = Database.open(${JSON.stringify(dir)}, { syncs: false });
    other.write(new Packr({ useRecords: false }).pack({ ops: [{ op: 'create', type: 'Task', id: 'Task-theirs' }], now: 1000 }));
    other.close();
  `);
  const changed = read(cursor);
  assert.deepEqual(changed.ids, ['Task-theirs']);
  assert.equal(changed.done, true);
  assert.equal(changed.complete, true);

  const unit = db.begin();
  const next = unit.nextSequence();
  unit.write(packr.pack({ ops: [{ op: 'create', type: 'Task', id: 'Task-unit' }], now: 1000 }));
  assert.deepEqual(packr.unpack(unit.changedSince(['Task'], packr.pack(changed.next), 100)).ids, [], 'stamped only as it commits');
  const committed = packr.unpack(unit.commit(false));
  assert.equal(committed.sequence, next);
  assert.deepEqual(read(changed.next).ids, ['Task-unit']);
});
