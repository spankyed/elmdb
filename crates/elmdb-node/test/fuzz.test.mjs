// Fuzzing the binding (docs/architecture/testing.md): wrong argument types, random bytes and random msgpack where a batch or a read
// goes, valid batches with bytes flipped, and a closed database and an ended unit. Each must throw a JavaScript error,
// one carrying an elmdb code where the payload reached elmdb, and leave the database working.
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
  [...readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../protocol/ErrorCode.ts'), 'utf8').matchAll(/"([A-Z_]+)"/g)].map((m) => m[1]),
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

function open(t) {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-fuzz-'));
  const db = Database.open(dir, {
    syncs: false,
    schema: { $schema: 'https://elmdb.dev/schema/1.json', relations: { refs: { cardinality: 'many-to-many' } }, types: { Note: { fields: { title: { type: 'string', optional: true } } } } },
  });
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

const valid = packr.pack({ ops: [
  { op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one' } },
  { op: 'create', type: 'Note', id: 'Note-2' },
  { op: 'link', source: 'Note-1', kind: 'refs', target: 'Note-2' },
] });
const validRead = packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, steps: [{ step: 'orderBy', field: 'title' }], terminal: { answer: 'rows' } } }] });

test('random bytes and random msgpack as batches, reads and loads throw elmdb codes', (t) => {
  const db = open(t);
  for (let seed = 1; seed <= 400; seed++) {
    const r = rng(seed);
    const bytes = Buffer.from(Array.from({ length: Math.floor(r() * 64) }, () => Math.floor(r() * 256)));
    const packed = packr.pack(junk(r));
    for (const payload of [bytes, packed]) {
      survives(() => db.write(payload), true, `seed ${seed} write`);
      survives(() => db.readSync(payload), true, `seed ${seed} read`);
      survives(() => db.bulkLoad(payload), true, `seed ${seed} load`);
    }
  }
  // Still working
  db.write(valid);
  assert.equal(packr.unpack(db.readSync(validRead))[0].rows.length, 2);
});

test('valid batches and reads with bytes flipped throw elmdb codes or apply', (t) => {
  const db = open(t);
  for (let seed = 1; seed <= 300; seed++) {
    const r = rng(seed);
    for (const source of [valid, validRead]) {
      const mutated = Buffer.from(source);
      const flips = 1 + Math.floor(r() * 3);
      for (let i = 0; i < flips; i++) mutated[Math.floor(r() * mutated.length)] = Math.floor(r() * 256);
      survives(() => (source === valid ? db.write(mutated) : db.readSync(mutated)), true, `seed ${seed}`);
    }
  }
  assert.deepEqual(db.check().differences, []);
});

test('arguments of the wrong type to every method throw rather than crash', (t) => {
  // A junk string is a relative path to `backup` and `replaceWith`, so they run from a directory of their own
  const cwd = process.cwd();
  const scratch = mkdtempSync(join(tmpdir(), 'elmdb-fuzz-cwd-'));
  process.chdir(scratch);
  t.after(() => {
    process.chdir(cwd);
    rmSync(scratch, { recursive: true, force: true });
  });
  const db = open(t);
  const unit = db.begin();
  const targets = [
    ['write', db], ['readSync', db], ['bulkLoad', db], ['backup', db], ['replaceWith', db], ['rebuild', db],
    ['purge', db], ['renameType', db], ['renameField', db], ['renameRelation', db],
    ['write', unit], ['read', unit], ['savepoint', unit], ['rollbackTo', unit], ['release', unit],
  ];
  for (let seed = 1; seed <= 200; seed++) {
    const r = rng(seed);
    for (const [name, target] of targets) {
      const args = Array.from({ length: Math.floor(r() * 3) }, () => junk(r));
      survives(() => target[name](...args), false, `seed ${seed} ${name}`);
    }
  }
  unit.abort();
  survives(() => Database.open(42), false, 'open with a number');
  survives(() => Database.open('/nonexistent/' + 'x'.repeat(5000), { readOnly: true }), true, 'open of nothing');
  db.write(valid);
});

test('a closed database and an ended unit refuse every call', (t) => {
  const db = open(t);
  const unit = db.begin();
  unit.abort();
  for (const call of [() => unit.write(valid), () => unit.read(validRead), () => unit.savepoint(), () => unit.commit()]) {
    assert.throws(call, { code: 'CLOSED' });
  }
  db.close();
  for (const call of [() => db.write(valid), () => db.readSync(validRead), () => db.begin(), () => db.check(), () => db.stats(), () => db.syncs]) {
    assert.throws(call, { code: 'CLOSED' });
  }
});
