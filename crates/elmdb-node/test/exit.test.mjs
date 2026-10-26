// A process that exits with a database still open leaves no lock behind: Node runs no destructor as it exits, so the
// binding closes what is open as its environment tears down. On macOS LMDB locks with named POSIX semaphores, which
// outlive the process unless the last one out removes them, and a table of them that fills refuses every later open on
// the machine (`ENOSPC`) until it restarts. The lock file holds what the names are made of; `sem_open` says whether one still exists.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const binding = fileURLToPath(new URL('../index.js', import.meta.url));
const { Database } = createRequire(import.meta.url)('../index.js');

/** LMDB's base-85 digits (`mdb_a85`, liblmdb's mdb.c) */
const A85 = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~';

/**
 * The named semaphores of the database at `dir`. LMDB keeps no names in its lock file, only the hash they are made from
 * (`mtb_mutexid`, a 64-bit word after the magic, the format, the last transaction and the reader count), and names the
 * reader and writer semaphores `/MDBr` and `/MDBw` followed by its base-85 digits, least significant first
 * (`mdb_env_mname_init`)
 */
function semaphoresOf(dir) {
  let id = readFileSync(join(dir, 'lock.mdb')).readBigUInt64LE(24);
  let digits = '';
  while (id > 0n && digits.length < 10) {
    digits += A85[Number(id % 85n)];
    id /= 85n;
  }
  return ['r', 'w'].map((kind) => `/MDB${kind}${digits}`);
}

/** Whether a named semaphore exists, asked of the C library through Python, which macOS ships */
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
  const db = Database.open(dir, { syncs: false });
  const names = semaphoresOf(dir);
  assert.deepEqual(names.map(exists), [true, true], `while open: ${names}`);
  db.close();
  assert.deepEqual(names.map(exists), [false, false]);
});

/** A child that opens the database at `dir`, reads from it so both semaphores are in use, then ends as `ends` says */
const endings = {
  'calls process.exit': (dir) => [
    `const { Database } = require(${JSON.stringify(binding)});`,
    `const db = Database.open(${JSON.stringify(dir)}, { syncs: false });`,
    `db.readSync(Buffer.from([0x81, 0xa5, 0x72, 0x65, 0x61, 0x64, 0x73, 0x90]));`,
    'process.exit(0);',
  ].join('\n'),
  'runs out of work': (dir) => [
    `const { Database } = require(${JSON.stringify(binding)});`,
    `const db = Database.open(${JSON.stringify(dir)}, { syncs: false });`,
    `db.readSync(Buffer.from([0x81, 0xa5, 0x72, 0x65, 0x61, 0x64, 0x73, 0x90]));`,
  ].join('\n'),
  'is a worker thread its parent terminates': (dir) => [
    `const { Worker } = require('node:worker_threads');`,
    `const worker = new Worker(${JSON.stringify([
      `const { Database } = require(${JSON.stringify(binding)});`,
      `const db = Database.open(${JSON.stringify(dir)}, { syncs: false });`,
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

test('a process that exits with a write group commit hasn\'t synced yet ends, its write on disk', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-exit-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const schema = { $schema: 'https://elmdb.dev/schema/1.json', types: { Note: { fields: {} } } };
  const batch = new Packr({ useRecords: false, mapsAsObjects: true })
    .pack({ ops: [{ op: 'create', type: 'Note', id: 'Note-1' }], now: 1000 });
  // On Windows `exit` ends the sync thread before the exit handlers run, so a close that waited on it never would end
  const child = [
    `const { Database } = require(${JSON.stringify(binding)});`,
    `const db = Database.open(${JSON.stringify(dir)}, { schema: ${JSON.stringify(schema)}, syncIdleMs: 60000, syncIntervalMs: 60000 });`,
    `db.write(Buffer.from(${JSON.stringify(batch.toString('base64'))}, 'base64'));`,
    'process.stdout.write(String(db.sequence));',
    'process.exit(0);',
  ].join('\n');
  const sequence = Number(execFileSync(process.execPath, ['-e', child], { encoding: 'utf8', timeout: 30_000 }));
  const db = Database.open(dir, { syncs: false });
  const reopened = db.sequence;
  db.close();
  assert.equal(reopened, sequence);
});
