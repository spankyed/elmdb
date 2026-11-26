// A process that exits with a database still open leaves no lock behind: Node runs no destructor as it exits, so the
// binding closes what is open as its environment or process tears down. On macOS LMDB locks with named POSIX semaphores
// which outlive the process unless `mdb_env_close` runs on close/atexit.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const binding = fileURLToPath(new URL('../index.js', import.meta.url));
const clientPath = fileURLToPath(new URL('../client.js', import.meta.url));
const { Database } = createRequire(import.meta.url)('../index.js');

const A85 = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~';

function semaphoresOf(dir) {
  let id = readFileSync(join(dir, 'lock.mdb')).readBigUInt64LE(24);
  let digits = '';
  while (id > 0n && digits.length < 10) {
    digits += A85[Number(id % 85n)];
    id /= 85n;
  }
  return ['r', 'w'].map((kind) => `/MDB${kind}${digits}`);
}

const exists = (name) => execFileSync('python3', ['-c', [
  'import ctypes, sys',
  'libc = ctypes.CDLL(None, use_errno=True)',
  'libc.sem_open.restype = ctypes.c_void_p',
  'handle = libc.sem_open(sys.argv[1].encode(), 0)',
  'print("yes" if handle not in (None, ctypes.c_void_p(-1).value) else "no")',
].join('\n'), name], { encoding: 'utf8' }).trim() === 'yes';

test('an open database holds the two semaphores its lock file names, and a close removes them', { skip: process.platform !== 'darwin' }, (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-sem-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const db = Database.open(dir, { durability: 'unsafe' });
  const names = semaphoresOf(dir);
  assert.deepEqual(names.map(exists), [true, true], `while open: ${names}`);
  db.close();
  assert.deepEqual(names.map(exists), [false, false]);
});

const endings = {
  'calls process.exit': (dir) => [
    `const { Database } = require(${JSON.stringify(binding)});`,
    `const db = Database.open(${JSON.stringify(dir)}, { durability: 'unsafe' });`,
    'db.stats();',
    'process.exit(0);',
  ].join('\n'),
  'runs out of work': (dir) => [
    `const { Database } = require(${JSON.stringify(binding)});`,
    `const db = Database.open(${JSON.stringify(dir)}, { durability: 'unsafe' });`,
    'db.stats();',
  ].join('\n'),
  'is a worker thread its parent terminates': (dir) => [
    `const { Worker } = require('node:worker_threads');`,
    `const worker = new Worker(${JSON.stringify([
      `const { Database } = require(${JSON.stringify(binding)});`,
      `const db = Database.open(${JSON.stringify(dir)}, { durability: 'unsafe' });`,
      'db.stats();',
      `require('node:worker_threads').parentPort.postMessage('open');`,
      'setInterval(() => {}, 1000);',
    ].join('\n'))}, { eval: true });`,
    `worker.once('message', () => worker.terminate());`,
  ].join('\n'),
};

for (const [ends, child] of Object.entries(endings)) {
  test(`a database left open where Node ${ends} leaves none of its lock semaphores`, { skip: process.platform !== 'darwin' }, (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'elmdb-exit-'));
    t.after(() => rmSync(dir, { recursive: true, force: true }));
    execFileSync(process.execPath, ['-e', child(dir)]);
    assert.deepEqual(semaphoresOf(dir).filter(exists), []);
  });
}

test('a process that exits with a grouped write persists it cleanly on exit', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-exit-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const schema = { types: { Note: { fields: { title: { type: 'string' } } } }, relations: {} };
  const child = [
    `const { Database } = require(${JSON.stringify(binding)});`,
    `const { buildWireBatch } = require(${JSON.stringify(clientPath)});`,
    `const db = Database.open(${JSON.stringify(dir)}, { schema: ${JSON.stringify(schema)}, durability: 'grouped' });`,
    `db.executeWireBatch(buildWireBatch((b) => { b.create('Note', { title: 'kept' }, 'Note-1'); }));`,
    'process.stdout.write(String(db.stats().commitSeq));',
    'process.exit(0);',
  ].join('\n');
  const seq = Number(execFileSync(process.execPath, ['-e', child], { encoding: 'utf8', timeout: 30_000 }));
  const db = Database.open(dir, { durability: 'unsafe' });
  const reopened = db.stats().commitSeq;
  const row = db.get('Note-1');
  db.close();
  assert.equal(reopened, seq);
  assert.equal(row.fields.title, 'kept');
});

test('handles that ran async jobs are collected by V8 once dropped', (t) => {
  const said = {};
  const calls = {
    query: `db.queryAsync({ source: { kind: 'allEntities' }, steps: [], terminal: { kind: 'count' } })`,
    flush: 'db.flushAsync()',
    check: 'db.checkAsync()',
    rebuild: 'db.rebuildAsync()',
  };
  for (const what of ['query', 'flush', 'check', 'rebuild', 'backup']) {
    const dir = mkdtempSync(join(tmpdir(), 'elmdb-exit-'));
    t.after(() => rmSync(dir, { recursive: true, force: true }));
    const child = [
      `const { Database } = require(${JSON.stringify(binding)});`,
      `let db = Database.open(${JSON.stringify(join(dir, 'db'))}, { durability: 'unsafe' });`,
      'let collected = false;',
      'const registry = new FinalizationRegistry(() => { collected = true; });',
      'registry.register(db, 0);',
      `const asked = ${what === 'backup' ? `db.backupAsync(${JSON.stringify(join(dir, 'copy'))})` : calls[what]};`,
      'db = undefined;',
      'asked.then(() => {',
      '  const until = Date.now() + 20000;',
      '  (function wait() {',
      '    global.gc();',
      '    if (!collected && Date.now() < until) return setImmediate(wait);',
      "    process.stdout.write(collected ? 'collected' : 'kept');",
      '  })();',
      '});',
    ].join('\n');
    said[what] = execFileSync(process.execPath, ['--expose-gc', '-e', child], { encoding: 'utf8', timeout: 60_000 });
  }
  assert.deepEqual(said, { query: 'collected', flush: 'collected', check: 'collected', rebuild: 'collected', backup: 'collected' });
});
