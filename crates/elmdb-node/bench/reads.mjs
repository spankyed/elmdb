// The read benchmark's four reads through the binding (`docs/roadmap/read-allocation-codec.md`): `node
// crates/elmdb-node/bench/reads.mjs` after `npm run build`. It builds the message store `tests/support/reads.rs`
// describes, checks each answer, then times `readSync` and the decoding of its answer apart, as medians of many runs.
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { tmpdir, cpus, totalmem, loadavg } from 'node:os';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import { Packr } from 'msgpackr';

const require = createRequire(import.meta.url);
const { Database } = require('../index.js');
const packr = new Packr({ useRecords: false, mapsAsObjects: true });

const args = process.argv.slice(2);
const json = args[0] === '--json';
if (json) args.shift();
let requestedRuns;
if (args.length) {
  assert.equal(args.shift(), '--runs', 'usage: reads.mjs [--json] [--runs N]');
  requestedRuns = Number(args.shift());
  assert.ok(Number.isSafeInteger(requestedRuns) && requestedRuns >= 1 && requestedRuns <= 100_000, 'runs must be 1..100000');
  assert.equal(args.length, 0, 'unexpected arguments');
}
const ROWS = 10_000;
const PER_CHANNEL = 100;
const WORDS = ['status', 'update', 'deploy', 'review', 'merge', 'branch', 'build', 'release'];
const id = (i) => `Message-${String(i).padStart(8, '0')}`;
const body = (i) => Array.from({ length: 150 }, (_, j) => WORDS[(i + j * 7) % WORDS.length]).join(' ');

function message(i) {
  const fields = {
    channel: `channel-${String(Math.floor(i / PER_CHANNEL)).padStart(4, '0')}`,
    title: `message ${i}`,
    sender: `sender-${i % 13}`,
    body: body(i),
    meta: { thread: `thread-${Math.floor(i / 10)}`, depth: i % 4, labels: ['inbox', `l${i % 5}`] },
    tags: [0, 1, 2].map((j) => `tag-${(i + j) % 17}`),
    attachment: Buffer.from(Array.from({ length: 64 }, (_, j) => (i * 31 + j) % 251)),
    score: i / 7,
    read: i % 2 === 0,
  };
  if (i % 5 !== 0) fields.note = i % 3 === 0 ? null : `note ${i}`;
  return fields;
}

const schema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  types: {
    Message: {
      fields: {
        channel: { type: 'string' }, title: { type: 'string' }, sender: { type: 'string' }, body: { type: 'string' },
        meta: { type: 'object' }, tags: { type: 'list', of: { type: 'string' } }, attachment: { type: 'bytes' },
        score: { type: 'number' }, read: { type: 'boolean' }, note: { type: 'string', optional: true, nullable: true },
      },
      indexes: { channel: { kind: 'value', fields: ['channel'] } },
    },
  },
};

// Source and binary identity are captured before measuring; invalid provenance fails before seeding.
let source, binary;
if (json) {
  const root = fileURLToPath(new URL('../../../', import.meta.url));
  const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8' }).trim();
  const suppliedCommit = process.env.ELMDB_PROFILE_SOURCE_COMMIT;
  const suppliedDirty = process.env.ELMDB_PROFILE_SOURCE_DIRTY;
  assert.equal(suppliedCommit === undefined, suppliedDirty === undefined, 'snapshot commit and dirty state must be supplied together');
  if (suppliedCommit !== undefined) {
    assert.match(suppliedCommit, /^[a-f0-9]{40}$/, 'snapshot commit must be a full Git SHA');
    assert.ok(suppliedDirty === 'true' || suppliedDirty === 'false', 'snapshot dirty state must be explicit');
  }
  source = suppliedCommit === undefined
    ? { commit: git('rev-parse', 'HEAD'), dirty: Boolean(git('status', '--porcelain', '--untracked-files=normal')), provenance: 'git' }
    : { commit: suppliedCommit, dirty: suppliedDirty === 'true', provenance: 'caller-provided snapshot' };
  const binaries = Object.keys(require.cache).filter((path) => path.endsWith('.node') && path.startsWith(fileURLToPath(new URL('../', import.meta.url))));
  assert.equal(binaries.length, 1, 'identify the actual loaded elmdb binary');

  binary = { name: binaries[0].split(/[\\/]/).at(-1), sha256: createHash('sha256').update(readFileSync(binaries[0])).digest('hex') };
}
const dir = mkdtempSync(join(tmpdir(), 'elmdb-read-bench-'));
let db;
try {
  db = Database.open(join(dir, 'db'), { schema, syncs: false });
  // Match the Rust fixture's imported rows: create() also adds timestamps that this fixture does not hold.
  const loads = Array.from({ length: ROWS }, (_, i) => ({ load: 'entity', id: id(i), fields: message(i), trashed: false }));
  db.bulkLoad(packr.pack(loads));

  const page = (terminal) => ({ reads: [{ read: 'query', plan: {
    start: { from: 'types', types: ['Message'] },
    steps: [{ step: 'where', field: 'channel', value: 'channel-0007' }, { step: 'limit', n: 50 }],
    terminal,
  } }] });
  const workloads = [
    ['field', { reads: [{ read: 'field', id: id(742), field: 'title' }] }, (a) => assert.deepEqual(a, { kind: 'field', present: true, value: 'message 742' })],
    ['page', page({ answer: 'rows' }), (a) => {
      assert.equal(a.kind, 'rows');
      assert.equal(a.rows.length, 50);
      a.rows.forEach((row, offset) => assert.deepEqual(row, { id: id(700 + offset), entityType: 'Message', ...message(700 + offset) }));
    }],
    ['page_projected', page({ answer: 'rows', fields: ['title', 'sender'] }), (a) => {
      assert.equal(a.kind, 'rows');
      assert.equal(a.rows.length, 50);
      a.rows.forEach((row, offset) => {
        const i = 700 + offset;
        assert.deepEqual(row, { id: id(i), title: `message ${i}`, sender: `sender-${i % 13}` });
      });
    }],
    ['all_rows', { reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Message'] }, terminal: { answer: 'rows' } } }] },
      (a) => {
        assert.equal(a.kind, 'rows');
        assert.equal(a.rows.length, ROWS);
        a.rows.forEach((row, i) => assert.deepEqual(row, { id: id(i), entityType: 'Message', ...message(i) }));
      }],
  ];

  const median = (xs) => xs.sort((a, b) => a - b)[Math.floor(xs.length / 2)];
  const measurements = [];
  for (const [name, reads, check] of workloads) {
    const request = packr.pack(reads);
    const checkedAnswer = db.readSync(request);
    check(packr.unpack(checkedAnswer)[0]);
    // Enough runs for a stable median at each size: the read of every row takes tens of milliseconds
    const runs = requestedRuns ?? (name === 'all_rows' ? 30 : 2000);
    const read = [], decode = [];
    for (let n = 0; n < runs; n++) {
      let t = process.hrtime.bigint();
      const answer = db.readSync(request);
      read.push(Number(process.hrtime.bigint() - t));
      t = process.hrtime.bigint();
      packr.unpack(answer);
      decode.push(Number(process.hrtime.bigint() - t));
    }
    measurements.push({ workload: name, answerBytes: checkedAnswer.length, answerSha256: createHash('sha256').update(checkedAnswer).digest('hex'), readSyncNs: read, decodeNs: decode });
    if (!json) console.log(`${name}: readSync_median_us=${(median(read) / 1000).toFixed(1)} decode_median_us=${(median(decode) / 1000).toFixed(1)} runs=${runs}`);
  }
  if (json) {
    console.log(JSON.stringify({
      schemaVersion: 1,
      measurement: 'node-read-and-decode',
      source,
      binary,
      runtime: { node: process.versions.node, napi: process.versions.napi, v8: process.versions.v8, platform: process.platform, arch: process.arch },
      host: { cpu: cpus()[0]?.model, logicalCpus: cpus().length, totalMemoryBytes: totalmem(), loadAverage: loadavg() },
      fixture: { version: 2, rows: ROWS, perChannel: PER_CHANNEL, syncs: false },
      policy: 'fixture and answer checks excluded; one warmup per workload; readSync and decode alternate; fresh process is not cold filesystem cache; no timing gate',
      workloads: measurements,
    }));
  }
} finally {
  try { db?.close(); } finally { rmSync(dir, { recursive: true, force: true }); }
}
