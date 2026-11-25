// Fuzzing the binding (docs/architecture/testing.md): wrong argument types, random bytes and random msgpack where a batch,
// read, position, checkpoint or load goes, byte-flipped valid payloads, pathological plans, deeply nested/malformed schemas,
// and a closed database and an ended unit. Each must return cleanly or throw a JavaScript error, one carrying an elmdb code
// where the payload reached elmdb, and leave the database working.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { Packr } from 'msgpackr';

const { Database } = createRequire(import.meta.url)('../index.js');
const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });
// Every code elmdb throws, read from the generated protocol
const codes = new Set(
  [...readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../protocol/ErrorCode.d.ts'), 'utf8').matchAll(/"([A-Z_]+)"/g)].map((m) => m[1]),
);

/** A small deterministic generator, so a failure names its seed */
function rng(seed) {
  let s = seed >>> 0 || 1;
  return () => ((s = (s * 1664525 + 1013904223) >>> 0) / 2 ** 32);
}

function junk(r, depth = 0) {
  const pick = Math.floor(r() * 9);
  switch (pick) {
    case 0: return null;
    case 1: return r() < 0.5;
    case 2: return Math.floor(r() * 1e6) - 5e5;
    case 3: return r() * 1e9;
    case 4: return 'x'.repeat(Math.floor(r() * 40));
    case 5: return depth > 3 ? [] : Array.from({ length: Math.floor(r() * 4) }, () => junk(r, depth + 1));
    case 6: return depth > 3 ? {} : Object.fromEntries(Array.from({ length: Math.floor(r() * 4) }, (_, i) => [['op', 'read', 'type', 'id', 'ops', 'reads', 'plan', 'fields', String(i)][Math.floor(r() * 9)], junk(r, depth + 1)]));
    case 7: return Buffer.from(Array.from({ length: Math.floor(r() * 32) }, () => Math.floor(r() * 256)));
    default: return undefined;
  }
}

const baseSchema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  relations: { refs: { cardinality: 'many-to-many' } },
  types: {
    Note: {
      changeOrder: true,
      fields: { title: { type: 'string', optional: true } },
      indexes: {
        grams: { kind: 'trigram', fields: ['title'] },
        meaning: { kind: 'vector', from: ['title'], dims: 3, model: 'm1' },
      },
    },
  },
};

function open(t, schema = baseSchema) {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-fuzz-'));
  const db = Database.open(dir, { syncs: false, schema });
  t.after(() => { db.close(); rmSync(dir, { recursive: true, force: true }); });
  return db;
}

/** Calls `fn`, which may succeed; a failure must be an Error, with an elmdb code when `coded` */
function survives(fn, coded, label) {
  try {
    fn();
  } catch (error) {
    assert.ok(error instanceof Error, `${label}: threw ${typeof error}`);
    if (coded) assert.ok(codes.has(error.code), `${label}: threw ${error.code}: ${error.message}`);
  }
}

/** Awaits `fn()`, which may resolve; a rejection or synchronous throw must be a structured Error */
async function survivesAsync(fn, coded, label) {
  try {
    await fn();
  } catch (error) {
    assert.ok(error instanceof Error, `${label}: rejected with ${typeof error}`);
    if (coded) assert.ok(codes.has(error.code), `${label}: rejected with ${error.code}: ${error.message}`);
  }
}

function nestedListSchema(depth) {
  let field = { type: 'string' };
  for (let i = 0; i < depth; i++) field = { type: 'list', of: field };
  return {
    $schema: 'https://elmdb.dev/schema/1.json',
    types: { Note: { fields: { nested: field } } },
  };
}

const valid = packr.pack({ ops: [
  { op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one' } },
  { op: 'create', type: 'Note', id: 'Note-2' },
  { op: 'link', source: 'Note-1', kind: 'refs', target: 'Note-2' },
] });
const validRead = packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, steps: [{ step: 'orderBy', field: 'title' }], terminal: { answer: 'rows' } } }] });
const validBulkLoad = packr.pack([
  { load: 'entity', id: 'Note-load-1', fields: { title: 'loaded' }, trashed: false },
  { load: 'entity', id: 'Note-load-2', fields: { title: 'second' }, trashed: false },
  { load: 'relation', id: 'Relation-load-1', kind: 'refs', source: 'Note-load-1', target: 'Note-load-2' },
]);
const validPosition = packr.pack({ sequence: 0 });

test('random bytes and random msgpack as batches, reads, positions, checkpoints and loads throw elmdb codes', async (t) => {
  const db = open(t);
  for (let seed = 1; seed <= 250; seed++) {
    const r = rng(seed);
    const bytes = Buffer.from(Array.from({ length: Math.floor(r() * 64) }, () => Math.floor(r() * 256)));
    const packed = packr.pack(junk(r));
    for (const payload of [bytes, packed]) {
      survives(() => db.write(payload), true, `seed ${seed} write`);
      survives(() => db.read(payload), true, `seed ${seed} read`);
      await survivesAsync(() => db.readAsync(payload), true, `seed ${seed} readAsync`);
      survives(() => db.bulkLoad(payload), true, `seed ${seed} load`);
      survives(() => db.changedSince(['Note'], payload, 16), true, `seed ${seed} changedSince`);
    }
  }
  // Unit byte boundaries: unit.write, unit.read, unit.changedSince, unit.setCheckpoint
  const unit = db.begin();
  for (let seed = 1; seed <= 120; seed++) {
    const r = rng(seed + 1000);
    const bytes = Buffer.from(Array.from({ length: Math.floor(r() * 64) }, () => Math.floor(r() * 256)));
    const packed = packr.pack(junk(r));
    for (const payload of [bytes, packed]) {
      survives(() => unit.write(payload), true, `seed ${seed} unit.write`);
      survives(() => unit.read(payload), true, `seed ${seed} unit.read`);
      survives(() => unit.changedSince(['Note'], payload, 16), true, `seed ${seed} unit.changedSince`);
      survives(() => unit.setCheckpoint(`cp-${seed}`, payload), true, `seed ${seed} unit.setCheckpoint`);
    }
  }
  unit.abort();
  // Position pre-decode validation: trailing bytes, over-deep nesting, non-UTF-8 string, invalid ext
  const badPositions = [
    Buffer.concat([validPosition, Buffer.from([0xc0])]),
    Buffer.concat([Buffer.alloc(260, 0x91), Buffer.from([0xc0])]),
    Buffer.from([0x81, 0xa4, 0x61, 0xed, 0xa0, 0x80, 0x00]),
    Buffer.from([0xc1]),
  ];
  for (const [i, pos] of badPositions.entries()) {
    assert.throws(() => db.changedSince(['Note'], pos, 10), { code: 'BATCH_INVALID' }, `bad position ${i}`);
  }
  // Still working
  db.write(valid);
  assert.equal(packr.unpack(db.read(validRead))[0].rows.length, 2);
  assert.equal(packr.unpack(await db.readAsync(validRead))[0].rows.length, 2);
});

test('valid batches, reads, positions and bulkLoad payloads with bytes flipped throw elmdb codes or apply', (t) => {
  const db = open(t);
  for (let seed = 1; seed <= 250; seed++) {
    const r = rng(seed);
    for (const [label, source] of [['batch', valid], ['read', validRead], ['position', validPosition]]) {
      const mutated = Buffer.from(source);
      const flips = 1 + Math.floor(r() * 3);
      for (let i = 0; i < flips; i++) mutated[Math.floor(r() * mutated.length)] = Math.floor(r() * 256);
      survives(() => {
        if (label === 'batch') db.write(mutated);
        else if (label === 'read') db.read(mutated);
        else db.changedSince(['Note'], mutated, 16);
      }, true, `seed ${seed} ${label}`);
    }
  }
  assert.deepEqual(db.check().differences, []);

  // Byte-flipped valid bulkLoad payloads in a dedicated database (bulkLoad intentionally skips schema checks)
  const bulkDb = open(t);
  for (let seed = 1; seed <= 200; seed++) {
    const r = rng(seed + 500);
    const mutated = Buffer.from(validBulkLoad);
    const flips = 1 + Math.floor(r() * 3);
    for (let i = 0; i < flips; i++) mutated[Math.floor(r() * mutated.length)] = Math.floor(r() * 256);
    survives(() => bulkDb.bulkLoad(mutated), true, `seed ${seed} bulkLoad flip`);
  }
  assert.ok(Array.isArray(bulkDb.check().differences));
  bulkDb.write(valid);
  assert.ok(packr.unpack(bulkDb.read(validRead))[0].rows.length >= 2);
});

test('pathological plan payloads throw structured elmdb errors or return cleanly', async (t) => {
  const db = open(t);
  db.write(valid);
  const start = { from: 'types', types: ['Note'] };
  const plans = [
    // Rank bounds
    { start, steps: [{ step: 'rank', kinds: ['refs'], iterations: 0 }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], iterations: 10001 }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], iterations: 0xffffffff }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], damping: NaN }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], damping: Infinity }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], damping: -0.5 }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], damping: 0 }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], damping: 1 }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], tolerance: NaN }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], tolerance: Infinity }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], tolerance: -1e-5 }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'rank', kinds: ['refs'], tolerance: 0 }], terminal: { answer: 'ids' } },
    // Communities bounds
    { start, terminal: { answer: 'communities', kinds: ['refs'], iterations: 0 } },
    { start, terminal: { answer: 'communities', kinds: ['refs'], iterations: 10001 } },
    { start, terminal: { answer: 'communities', kinds: ['refs'], iterations: 0xffffffff } },
    { start, terminal: { answer: 'communities', kinds: ['refs'], resolution: NaN } },
    { start, terminal: { answer: 'communities', kinds: ['refs'], resolution: Infinity } },
    { start, terminal: { answer: 'communities', kinds: ['refs'], resolution: -1 } },
    // Limit edge numbers
    { start, steps: [{ step: 'limit', n: NaN }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'limit', n: Infinity }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'limit', n: -Infinity }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'limit', n: -5 }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'limit', n: 0 }], terminal: { answer: 'ids' } },
    // Step chains > MAX_STEPS (256)
    { start, steps: Array.from({ length: 257 }, () => ({ step: 'reverse' })), terminal: { answer: 'ids' } },
    // Invalid regexes in Matching
    { start, steps: [{ step: 'matching', pattern: '(unclosed[' }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'matching', pattern: 'foo(?=bar)' }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'matching', pattern: '(a)\\1' }], terminal: { answer: 'ids' } },
    // Oversized, empty, and non-finite vectors in Near
    { start, steps: [{ step: 'near', vector: Array.from({ length: 256 }, () => 1.0), k: 5 }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'near', vector: [], k: 5 }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'near', vector: [NaN, 0, 0], k: 5 }], terminal: { answer: 'ids' } },
    { start, steps: [{ step: 'near', vector: [Infinity, -Infinity, 0], k: 5 }], terminal: { answer: 'ids' } },
  ];
  for (const [i, plan] of plans.entries()) {
    const payload = packr.pack({ reads: [{ read: 'query', plan }] });
    survives(() => db.read(payload), true, `pathological plan ${i} read`);
    await survivesAsync(() => db.readAsync(payload), true, `pathological plan ${i} readAsync`);
  }
  assert.deepEqual(db.check().differences, []);
});

test('deeply nested and malformed schemas throw SCHEMA_INVALID on setSchema and open', (t) => {
  const db = open(t);
  // Depth 32 passes on open; depth 33 and 100 fail with SCHEMA_INVALID on both open and setSchema
  const dir32 = mkdtempSync(join(tmpdir(), 'elmdb-fuzz-s32-'));
  t.after(() => rmSync(dir32, { recursive: true, force: true }));
  const db32 = Database.open(dir32, { syncs: false, schema: nestedListSchema(32) });
  db32.close();

  for (const depth of [33, 64]) {
    const deep = nestedListSchema(depth);
    assert.throws(() => db.setSchema(deep), { code: 'SCHEMA_INVALID' }, `setSchema depth ${depth}`);
    const dir = mkdtempSync(join(tmpdir(), `elmdb-fuzz-s${depth}-`));
    t.after(() => rmSync(dir, { recursive: true, force: true }));
    assert.throws(() => Database.open(dir, { syncs: false, schema: deep }), { code: 'SCHEMA_INVALID' }, `open depth ${depth}`);
  }
  // Even deeper nesting (>= 128) is also rejected cleanly before reaching the schema parser
  const veryDeep = nestedListSchema(128);
  assert.throws(() => db.setSchema(veryDeep), Error, 'setSchema depth 128');
  const dir128 = mkdtempSync(join(tmpdir(), 'elmdb-fuzz-s128-'));
  t.after(() => rmSync(dir128, { recursive: true, force: true }));
  assert.throws(() => Database.open(dir128, { syncs: false, schema: veryDeep }), Error, 'open depth 128');

  for (let seed = 1; seed <= 80; seed++) {
    const r = rng(seed + 2000);
    const bad = junk(r);
    survives(() => db.setSchema(bad), false, `seed ${seed} setSchema`);
    const dir = mkdtempSync(join(tmpdir(), 'elmdb-fuzz-open-'));
    t.after(() => rmSync(dir, { recursive: true, force: true }));
    survives(() => {
      const opened = Database.open(dir, { syncs: false, schema: bad });
      opened.close();
    }, false, `seed ${seed} open schema`);
  }
  // Database remains healthy and usable
  db.write(valid);
  assert.deepEqual(db.check().differences, []);
});

test('numeric, checkpoint and wrong-type arguments to every method throw rather than crash', async (t) => {
  // A junk string is a relative path to `backup` and `replaceWith`, so they run from a directory of their own
  const cwd = process.cwd();
  const scratch = mkdtempSync(join(tmpdir(), 'elmdb-fuzz-cwd-'));
  process.chdir(scratch);
  t.after(() => {
    process.chdir(cwd);
    rmSync(scratch, { recursive: true, force: true });
  });
  const db = open(t);
  for (const n of [-1, 0, 1, 1.5, NaN, Infinity, -Infinity, 0xffffffff, 0x100000000, Number.MAX_SAFE_INTEGER + 2]) {
    survives(() => db.adoptReplacement(n), true, `adoptReplacement(${n})`);
    survives(() => db.untrack(n), true, `untrack(${n})`);
  }
  for (const name of ['', 'cp', '\u0000', 'x'.repeat(2048), 'é🎉']) {
    survives(() => db.checkpoint(name), true, `checkpoint(${name.slice(0, 8)})`);
  }
  const unit = db.begin();
  const targets = [
    ['write', db], ['read', db], ['bulkLoad', db], ['changedSince', db], ['checkpoint', db],
    ['setSchema', db], ['adoptReplacement', db], ['untrack', db],
    ['backup', db], ['replaceWith', db], ['rebuild', db],
    ['purge', db], ['renameType', db], ['renameField', db], ['renameRelation', db],
    ['write', unit], ['read', unit], ['changedSince', unit], ['setCheckpoint', unit], ['checkpoint', unit],
    ['savepoint', unit], ['rollbackTo', unit], ['release', unit],
  ];
  for (let seed = 1; seed <= 150; seed++) {
    const r = rng(seed);
    for (const [name, target] of targets) {
      const args = Array.from({ length: Math.floor(r() * 4) }, () => junk(r));
      survives(() => target[name](...args), false, `seed ${seed} ${name}`);
    }
    const asyncArgs = Array.from({ length: Math.floor(r() * 3) }, () => junk(r));
    await survivesAsync(() => db.readAsync(...asyncArgs), false, `seed ${seed} readAsync`);
  }
  unit.abort();
  survives(() => Database.open(42), false, 'open with a number');
  survives(() => Database.open('/nonexistent/' + 'x'.repeat(5000), { readOnly: true }), true, 'open of nothing');
  db.write(valid);
  assert.deepEqual(db.check().differences, []);
});

test('a closed database and an ended unit refuse every call', async (t) => {
  const db = open(t);
  const unit = db.begin();
  unit.abort();
  for (const call of [
    () => unit.write(valid),
    () => unit.read(validRead),
    () => unit.changedSince(['Note'], validPosition, 10),
    () => unit.setCheckpoint('cp', packr.pack(1)),
    () => unit.savepoint(),
    () => unit.commit(),
  ]) {
    assert.throws(call, { code: 'CLOSED' });
  }
  db.close();
  for (const call of [
    () => db.write(valid),
    () => db.read(validRead),
    () => db.changedSince(['Note'], validPosition, 10),
    () => db.checkpoint('cp'),
    () => db.setSchema(baseSchema),
    () => db.adoptReplacement(0),
    () => db.begin(),
    () => db.check(),
    () => db.stats(),
    () => db.syncs,
  ]) {
    assert.throws(call, { code: 'CLOSED' });
  }
  // Calls answering promises refuse by rejecting them
  await assert.rejects(db.durable(), { code: 'CLOSED' });
  await assert.rejects(db.readAsync(validRead), { code: 'CLOSED' });
});
