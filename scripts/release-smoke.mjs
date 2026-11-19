// The release's npm package installed as a user installs it, into an empty project on the platform this runs on, and
// a database run through it: `node scripts/release-smoke.mjs <elmdb-x.y.z.tgz>`. Nothing of this checkout is loaded,
// so what answers is the binary the package carries for this platform, found by napi's loader in the package's own
// directory. It fails when the package holds no binary for this platform, when that binary doesn't load on this
// system's libc or OS release, or when the database answers wrongly. Run by `.github/workflows/release.yml` on each
// target, with the Node the package's `engines` names as its floor.
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

const tarball = process.argv[2];
if (!tarball || !fs.existsSync(tarball)) {
  console.error('usage: node scripts/release-smoke.mjs <elmdb-x.y.z.tgz>');
  process.exit(2);
}

const project = fs.mkdtempSync(path.join(os.tmpdir(), 'elmdb-release-smoke-'));
fs.writeFileSync(path.join(project, 'package.json'), JSON.stringify({ private: true, type: 'module' }));
// msgpackr is the codec the README's example writes with; elmdb itself depends on nothing
execFileSync('npm', ['install', '--no-audit', '--no-fund', '--loglevel=error', path.resolve(tarball), 'msgpackr@2.1.4'], {
  cwd: project,
  stdio: 'inherit',
  shell: process.platform === 'win32',
});

// A text index exercises the tokenizer and the collation compiled into the binary, a relation and a rank the graph
// code, and `check()` every derived key against the primary data the write left
const program = `
import { Database } from 'elmdb';
import { Packr } from 'msgpackr';
import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import { createRequire } from 'node:module';

const loaded = createRequire(import.meta.url).resolve('elmdb');
assert.ok(loaded.startsWith(fs.realpathSync('node_modules/elmdb')), 'elmdb resolved outside the installed package: ' + loaded);
const msgpack = new Packr({ useRecords: false, mapsAsObjects: true, int64AsType: 'auto' });
const db = Database.open('db', {
  schema: {
    $schema: 'https://elmdb.dev/schema/1.json',
    types: { Note: { fields: { title: { type: 'string' } }, indexes: { text: { kind: 'text', fields: { title: {} } } } } },
    relations: { refs: { cardinality: 'many-to-many' } },
    ranks: { noteRank: { types: ['Note'], relations: ['refs'] } },
  },
});
db.write(msgpack.pack({ ops: [
  { op: 'create', type: 'Note', id: 'Note-a', fields: { title: 'Choosing a database' } },
  { op: 'create', type: 'Note', id: 'Note-b', fields: { title: 'Backups' } },
  { op: 'create', type: 'Note', id: 'Note-c', fields: { title: 'Database migrations' } },
  { op: 'link', source: 'Note-b', kind: 'refs', target: 'Note-a' },
  { op: 'link', source: 'Note-c', kind: 'refs', target: 'Note-a' },
] }));
const notes = { from: 'types', types: ['Note'] };
const [found, top] = msgpack.unpack(db.read(msgpack.pack({ reads: [
  { read: 'query', plan: { start: notes, steps: [{ step: 'search', query: 'databse' }], terminal: { answer: 'ids' } } },
  { read: 'query', plan: { start: notes, steps: [{ step: 'ranked', index: 'noteRank' }, { step: 'limit', n: 1 }], terminal: { answer: 'ids' } } },
] })));
assert.deepEqual([...found.ids].sort(), ['Note-a', 'Note-c']);
assert.deepEqual(top.ids, ['Note-a']);
const report = db.check();
assert.deepEqual([report.differences, report.violations], [[], []], JSON.stringify(report));
db.close();
console.log('elmdb from ' + loaded + ' answered on ' + process.platform + '-' + process.arch + ', Node ' + process.version);
`;
fs.writeFileSync(path.join(project, 'smoke.mjs'), program);
try {
  execFileSync(process.execPath, ['smoke.mjs'], { cwd: project, stdio: 'inherit' });
} finally {
  fs.rmSync(project, { recursive: true, force: true });
}
