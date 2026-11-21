// The release's two npm packages installed as a user installs them, into an empty project on the platform this runs on,
// and a database run through each: `node scripts/release-smoke.mjs <elmdb-x.y.z.tgz> <elmdb-ears-x.y.z.tgz>`. Nothing
// of this checkout is loaded, so what answers is the binary the binding's package carries for this platform, found by
// napi's loader in the package's own directory, and EARS' built `dist`, resolving `elmdb` as its peer. It fails when
// the binding's package holds no binary for this platform, when that binary doesn't load on this system's libc or OS
// release, when npm won't install EARS beside that binding, or when either answers wrongly. Run by
// `.github/workflows/release.yml` on each target, with the Node the packages' `engines` name as their floor.
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

const [tarball, earsTarball] = process.argv.slice(2);
if (![tarball, earsTarball].every((file) => file && fs.existsSync(file))) {
  console.error('usage: node scripts/release-smoke.mjs <elmdb-x.y.z.tgz> <elmdb-ears-x.y.z.tgz>');
  process.exit(2);
}

const project = fs.mkdtempSync(path.join(os.tmpdir(), 'elmdb-release-smoke-'));
fs.writeFileSync(path.join(project, 'package.json'), JSON.stringify({ private: true, type: 'module' }));
// msgpackr is the codec the README's example writes with; elmdb itself depends on nothing, and EARS on msgpackr, which
// npm resolves from the registry. Both packages in one install, as a user adds them: npm refuses EARS unless the
// binding beside it is the version its peer names
const packages = [path.resolve(tarball), path.resolve(earsTarball), 'msgpackr@2.1.4'];
execFileSync('npm', ['install', '--no-audit', '--no-fund', '--loglevel=error', '--strict-peer-deps', ...packages], {
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
// EARS as its README's example runs it, through its two entries: the builders compile to plans and batches, and
// `@elmdb/ears/database` opens the database through the binding installed beside it, which is the one the program
// above loaded. The thin client and the package's own manifest are read through the binding's `exports`
const earsProgram = `
import { createEarsEngine } from '@elmdb/ears';
import { openDatabase } from '@elmdb/ears/database';
import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const ears = fs.realpathSync('node_modules/@elmdb/ears');
assert.ok(require.resolve('@elmdb/ears/database').startsWith(ears), 'the EARS database entry resolved outside the installed package');
assert.ok(require.resolve('elmdb', { paths: [ears] }).startsWith(fs.realpathSync('node_modules/elmdb')),
  'EARS resolved an elmdb other than the one installed beside it');
assert.equal(require('elmdb/package.json').version, require('@elmdb/ears/package.json').peerDependencies.elmdb);
assert.equal(typeof require('elmdb/client.js').Client, 'function');
const database = openDatabase({
  path: 'ears-db',
  schema: {
    types: { Note: { required: ['title'], search: { fields: { title: 1 } } } },
    relations: { refs: {} },
    roles: [],
  },
});
const engine = createEarsEngine({ database });
const choosing = engine.create('Note', { title: 'Choosing a database' }).id();
engine.create('Note', { title: 'Backups' }).link('refs', choosing);
engine.create('Note', { title: 'Database migrations' }).link('refs', choosing);
const titles = (notes) => notes.map((note) => note.title);
assert.deepEqual(titles(engine.query('Note').search('databse').pick(['title'])), ['Database migrations', 'Choosing a database']);
assert.deepEqual(titles(engine.query.ids([choosing]).in('refs', 'Note').pick(['title'])).sort(), ['Backups', 'Database migrations']);
database.close();
console.log('@elmdb/ears from ' + ears + ' answered on ' + process.platform + '-' + process.arch + ', Node ' + process.version);
`;
fs.writeFileSync(path.join(project, 'smoke.mjs'), program);
fs.writeFileSync(path.join(project, 'ears-smoke.mjs'), earsProgram);
try {
  execFileSync(process.execPath, ['smoke.mjs'], { cwd: project, stdio: 'inherit' });
  execFileSync(process.execPath, ['ears-smoke.mjs'], { cwd: project, stdio: 'inherit' });
} finally {
  fs.rmSync(project, { recursive: true, force: true });
}
