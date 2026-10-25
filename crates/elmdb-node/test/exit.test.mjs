// A process that exits with a database still open leaves no lock behind: Node runs no destructor as it exits, so the
// binding closes what is open as its environment tears down. On macOS LMDB locks with named POSIX semaphores, which
// outlive the process unless the last one out removes them, and a table of them that fills refuses every later open on
// the machine (`ENOSPC`) until it restarts. The names are in the lock file; `sem_open` says whether one still exists.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const binding = fileURLToPath(new URL('../index.js', import.meta.url));

/** The named semaphores the lock file of the database at `dir` names */
const semaphoresOf = (dir) => [...readFileSync(join(dir, 'lock.mdb')).toString('latin1').matchAll(/\/MDB[rw][!-~]+/g)].map(([name]) => name);

/** Whether a named semaphore exists, asked of the C library through Python, which macOS ships */
const exists = (name) => execFileSync('python3', ['-c', [
  'import ctypes, sys',
  'libc = ctypes.CDLL(None, use_errno=True)',
  'libc.sem_open.restype = ctypes.c_void_p',
  'handle = libc.sem_open(sys.argv[1].encode(), 0)',
  'print("yes" if handle not in (None, ctypes.c_void_p(-1).value) else "no")',
].join('\n'), name], { encoding: 'utf8' }).trim() === 'yes';

test('a process that exits with a database open leaves none of its lock semaphores', { skip: process.platform !== 'darwin' }, (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-exit-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const child = [
    `const { Database } = require(${JSON.stringify(binding)});`,
    `const db = Database.open(${JSON.stringify(dir)}, { syncs: false });`,
    // A read registers a reader, so both semaphores are in use as the process leaves
    `db.readSync(Buffer.from([0x81, 0xa5, 0x72, 0x65, 0x61, 0x64, 0x73, 0x90]));`,
    'process.exit(0);',
  ].join('\n');
  execFileSync(process.execPath, ['-e', child]);
  const names = semaphoresOf(dir);
  assert.equal(names.length, 2, `the lock file names a reader and a writer semaphore: ${names}`);
  assert.deepEqual(names.filter(exists), []);
});
