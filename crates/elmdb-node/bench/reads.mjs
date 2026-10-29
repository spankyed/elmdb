// The read benchmark's four reads through the binding (`docs/roadmap/read-allocation-codec.md`): `node
// crates/elmdb-node/bench/reads.mjs` after `npm run build`. It builds the message store `tests/support/reads.rs`
// describes, checks each answer, then times `readSync` and the decoding of its answer apart, as medians of many runs.
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Packr } from 'msgpackr';

const require = createRequire(import.meta.url);
const { Database } = require('../index.js');
const packr = new Packr({ useRecords: false, mapsAsObjects: true });

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

const dir = mkdtempSync(join(tmpdir(), 'elmdb-read-bench-'));
const db = Database.open(join(dir, 'db'), { schema, syncs: false });
for (let start = 0; start < ROWS; start += 1000) {
  const ops = [];
  for (let i = start; i < start + 1000; i++) ops.push({ op: 'create', type: 'Message', id: id(i), fields: message(i) });
  db.write(packr.pack({ ops }));
}

const page = (terminal) => ({ reads: [{ read: 'query', plan: {
  start: { from: 'types', types: ['Message'] },
  steps: [{ step: 'where', field: 'channel', value: 'channel-0007' }, { step: 'limit', n: 50 }],
  terminal,
} }] });
const workloads = [
  ['field', { reads: [{ read: 'field', id: id(742), field: 'title' }] }, (a) => assert.equal(a.value, 'message 742')],
  ['page', page({ answer: 'rows' }), (a) => assert.equal(a.rows.length, 50)],
  ['page_projected', page({ answer: 'rows', fields: ['title', 'sender'] }), (a) => assert.deepEqual(Object.keys(a.rows[0]), ['id', 'title', 'sender'])],
  ['all_rows', { reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Message'] }, terminal: { answer: 'rows' } } }] },
    (a) => assert.equal(a.rows.length, ROWS)],
];

const median = (xs) => xs.sort((a, b) => a - b)[Math.floor(xs.length / 2)];
for (const [name, reads, check] of workloads) {
  const request = packr.pack(reads);
  check(packr.unpack(db.readSync(request))[0]);
  // Enough runs for a stable median at each size: the read of every row takes tens of milliseconds
  const runs = name === 'all_rows' ? 30 : 2000;
  const read = [], decode = [];
  for (let n = 0; n < runs; n++) {
    let t = process.hrtime.bigint();
    const answer = db.readSync(request);
    read.push(Number(process.hrtime.bigint() - t));
    t = process.hrtime.bigint();
    packr.unpack(answer);
    decode.push(Number(process.hrtime.bigint() - t));
  }
  console.log(`${name}: readSync_median_us=${(median(read) / 1000).toFixed(1)} decode_median_us=${(median(decode) / 1000).toFixed(1)} runs=${runs}`);
}
db.close();
rmSync(dir, { recursive: true, force: true });
