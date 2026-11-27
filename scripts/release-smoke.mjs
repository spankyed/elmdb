// The binding and EARS installed as a user installs them, into an empty project on the platform this runs on, and a
// database run through each:
//
//   node scripts/release-smoke.mjs <elmdb-x.y.z.tgz> <elmdb-ears-x.y.z.tgz> <elmdb-binding-<platform>-x.y.z.tgz>...
//   node scripts/release-smoke.mjs <elmdb's URL> <EARS' URL>
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

const [tarball, earsTarball, ...platformTarballs] = process.argv.slice(2);
const isUrl = (spec) => /^https:\/\//.test(spec ?? '');
const specs = [tarball, earsTarball, ...platformTarballs];
if (!specs.every((spec) => spec && (isUrl(spec) || fs.existsSync(spec))) || platformTarballs.some(isUrl)) {
  console.error('usage: node scripts/release-smoke.mjs <elmdb-x.y.z.tgz | URL> <elmdb-ears-x.y.z.tgz | URL> [<elmdb-binding-<platform>-x.y.z.tgz>...]');
  process.exit(2);
}
const resolved = (spec) => (isUrl(spec) ? spec : path.resolve(spec));

const libc = process.platform !== 'linux' ? '' : process.report.getReport().header.glibcVersionRuntime ? '-gnu' : '-musl';
const platform = `binding-${process.platform}-${process.arch}${libc}${process.platform === 'win32' ? '-msvc' : ''}`;
const expected = `@elmdb/${platform}`;

const overrides = Object.fromEntries(
  platformTarballs.map((file) => {
    const manifest = JSON.parse(execFileSync('tar', ['-xOzf', file, 'package/package.json'], { encoding: 'utf8' }));
    return [manifest.name, `file:${path.resolve(file).replaceAll('\\', '/')}`];
  }),
);
if (platformTarballs.length > 0 && !(expected in overrides)) {
  throw new Error(`No tarball given is ${expected}'s, the platform package this machine installs`);
}

const project = fs.mkdtempSync(path.join(os.tmpdir(), 'elmdb-release-smoke-'));
fs.writeFileSync(path.join(project, 'package.json'), JSON.stringify({ private: true, type: 'module', overrides }));
// elmdb v2 and @elmdb/ears v2 have zero runtime dependencies (no msgpackr!).
const packages = [resolved(tarball), resolved(earsTarball)];
execFileSync('npm', ['install', '--no-audit', '--no-fund', '--loglevel=error', '--strict-peer-deps', ...packages], {
  cwd: project,
  stdio: 'inherit',
  shell: process.platform === 'win32',
});

const installed = [
  ...fs.readdirSync(path.join(project, 'node_modules')).filter((name) => /^elmdb-/.test(name)),
  ...fs.readdirSync(path.join(project, 'node_modules', '@elmdb')).filter((name) => /^binding-/.test(name)).map((name) => `@elmdb/${name}`),
];
if (installed.length !== 1 || installed[0] !== expected) {
  throw new Error(`npm installed [${installed.join(', ')}] beside elmdb, and this machine's platform package is ${expected}`);
}
const strays = fs.readdirSync(path.join(project, 'node_modules', 'elmdb')).filter((file) => file.endsWith('.node'));
if (strays.length > 0) throw new Error(`elmdb's own package holds ${strays.join(', ')}; its binaries are the platform packages'`);
console.log(`npm installed ${expected} alone beside elmdb`);

const program = `
import { Database } from 'elmdb';
import { buildWireBatch } from 'elmdb/client';
import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import { createRequire } from 'node:module';
import * as path from 'node:path';

const require = createRequire(import.meta.url);
const loaded = require.resolve('elmdb');
assert.ok(loaded.startsWith(fs.realpathSync('node_modules/elmdb')), 'elmdb resolved outside the installed package: ' + loaded);
const db = Database.open('db', {
  buildPolicy: 'eager',
  schema: {
    $schema: 'https://elmdb.dev/schema/2.json',
    types: {
      Note: {
        fields: { title: { type: 'string' } },
        indexes: {
          text: { kind: 'text', fields: [['title', 1.0]] },
          noteRank: { kind: 'rank', relations: ['refs'] },
        },
      },
    },
    relations: {
      refs: {
        cardinality: 'manyToMany',
        endpoints: [{ source: 'Note', target: 'Note' }],
      },
    },
  },
});
db.executeWireBatch(buildWireBatch((b) => {
  const a = b.create('Note', { title: 'Choosing a database' }, 'Note-a');
  const bNode = b.create('Note', { title: 'Backups' }, 'Note-b');
  const cNode = b.create('Note', { title: 'Database migrations' }, 'Note-c');
  b.link(bNode, 'refs', a);
  b.link(cNode, 'refs', a);
}));
const found = db.query({
  source: { kind: 'type', typeName: 'Note' },
  steps: [{ kind: 'score', spec: { kind: 'bm25f', index: 'text', query: 'database', topK: 10 } }],
  terminal: { kind: 'ids' },
});
assert.deepEqual([...found.ids].sort(), ['Note-a', 'Note-c']);
const report = db.check();
assert.equal(report.clean, true, JSON.stringify(report));
db.close();
const platformPackage = fs.realpathSync('node_modules/${expected}');
const binaries = Object.keys(require.cache).filter((file) => /^elmdb\\..*\\.node$/.test(path.basename(file)));
assert.ok(binaries.length === 1 && binaries[0].startsWith(platformPackage), 'the binary loaded is ' + binaries.join(', ') + ', and is to be ${expected}' + "'s");
console.log('elmdb from ' + loaded + ' with ' + binaries[0] + ' answered on ' + process.platform + '-' + process.arch + ', Node ' + process.version);
`;

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
assert.equal(typeof require('elmdb/client').WireBatchBuilder, 'function');
const database = openDatabase({
  path: 'ears-db',
  buildPolicy: 'eager',
  schema: {
    types: {
      Note: {
        fields: { title: { type: 'string' } },
        indexes: { text: { kind: 'text', fields: [['title', 1.0]] } },
      },
    },
    relations: {
      refs: {
        cardinality: 'manyToMany',
        endpoints: [{ source: 'Note', target: 'Note' }],
      },
    },
  },
});
const engine = createEarsEngine({ database });
engine.batch((b) => {
  const choosing = b.create('Note', { title: 'Choosing a database' }, 'note-a');
  const backups = b.create('Note', { title: 'Backups' }, 'note-b');
  const migrations = b.create('Note', { title: 'Database migrations' }, 'note-c');
  b.link(backups, 'refs', choosing);
  b.link(migrations, 'refs', choosing);
});
const found = engine.query.of('Note').search('text', 'database', { topK: 10 }).ids();
assert.deepEqual([...found].sort(), ['note-a', 'note-c']);
// Prefix BM25F search + UTF-16 highlights + projected fields
const prefixHits = engine.query
  .of('Note')
  .search('text', 'datab', { prefix: true, topK: 10 })
  .hits({ fields: ['title'], highlights: true });
assert.equal(prefixHits.length, 2);
for (const h of prefixHits) {
  const title = h.row?.fields.title;
  assert.equal(typeof title, 'string');
  const spans = h.highlights?.title ?? [];
  assert.ok(spans.length > 0, 'expected highlight spans on title');
  for (const [itemIdx, start, end] of spans) {
    assert.equal(itemIdx, 0);
    assert.equal(title.slice(start, end).toLowerCase(), 'database');
  }
}
// Transact with nested savepoint rollback, RYOW, persistent counters, and beforeCommit hook
let hookCount = 0;
const unsubHook = engine.beforeCommit(['Note'], () => {
  hookCount++;
});
const watchCounts = [];
const watchHandle = engine.query
  .of('Note')
  .watch((rows) => watchCounts.push(rows.length), { fields: ['title'], minGapMs: 0, adaptiveMultiplier: 0 });
assert.deepEqual(watchCounts, [3]);
engine.transact(() => {
  assert.equal(engine.nextCounter('NOTE'), 1);
  engine.create('Note', { title: 'Transactional note' }, 'note-d');
  assert.equal(engine.get('note-d')?.fields.title, 'Transactional note');
  assert.throws(() => {
    engine.transact(() => {
      engine.create('Note', { title: 'Rolled back note' }, 'note-rb');
      assert.equal(engine.nextCounter('NOTE'), 2);
      throw new Error('rollback inner');
    });
  }, /rollback inner/);
  assert.equal(engine.get('note-rb'), null);
  assert.equal(engine.getCounter('NOTE'), 1);
});
assert.equal(hookCount, 1);
engine.watches.flushSync(Date.now() + 60_000);
assert.equal(watchCounts[watchCounts.length - 1], 4);
watchHandle.stop();
unsubHook();
const incoming = engine.query.ids(['note-a']).hops({ relKinds: ['refs'], dir: 'in', maxHops: 1 }).all();
assert.deepEqual(incoming.map((r) => r.fields.title).sort(), ['Backups', 'Database migrations']);
engine.admin.close();
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
