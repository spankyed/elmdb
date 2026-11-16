// @ts-check
// `npm run typecheck` checks this file against the client's declarations, and `npm test` runs it: the client's calls
// take and answer the protocol's generated types, and an answer that may hold `__proto__` keeps it as a key
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
/** @type {typeof import('../index.js')} */
const { Database } = require('../index.js');
/** @type {typeof import('../client.js')} */
const { Client } = require('../client.js');
/** @type {{ Packr: new (options: object) => { pack(value: unknown): Buffer, unpack(bytes: Buffer): unknown } }} */
const { Packr } = require('msgpackr');

const objects = new Packr({ useRecords: false, mapsAsObjects: true });
const maps = new Packr({ useRecords: false, mapsAsObjects: false });
/** @type {import('../client.js').Codec} */
const codec = {
  pack: (value) => objects.pack(value),
  unpack: (bytes) => objects.unpack(bytes),
  unpackMaps: (bytes) => maps.unpack(bytes),
};

/** @param {import('node:test').TestContext} t */
function open(t) {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  const db = Database.open(dir, {
    syncs: false,
    schema: {
      $schema: 'https://elmdb.dev/schema/1.json',
      types: { Note: { fields: { title: { type: 'string' }, meta: { type: 'any', optional: true } } } },
    },
  });
  // Closed before its directory is removed: Windows won't remove a file that is mapped
  t.after(() => {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  });
  return new Client(db, codec);
}

/** @type {import('../index.js').Start} */
const notes = { from: 'types', types: ['Note'] };

test('a write and a read take and answer the generated types', async (t) => {
  const client = open(t);
  const written = client.write({ ops: [{ op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one' } }], now: 1000 });
  assert.deepEqual(written.results, [{ kind: 'entity', id: 'Note-1', shortCode: null, created: true }]);
  /** @type {import('../index.js').Reads} */
  const reads = { reads: [
    { read: 'query', plan: { start: notes, terminal: { answer: 'rows', fields: ['title'] } } },
    { read: 'query', plan: { start: notes, terminal: { answer: 'count' } } },
  ] };
  const [rows, count] = client.read(reads);
  assert.ok(rows.kind === 'rows' && count.kind === 'count');
  // Read through its type: a row's id is a string, and `n` a number
  assert.deepEqual(rows.rows.map((row) => row.id), ['Note-1']);
  assert.deepEqual(rows, { kind: 'rows', rows: [{ id: 'Note-1', title: 'one' }], plainObjects: true });
  assert.equal(count.n + 1, 2);
  assert.deepEqual(await client.readAsync(reads), [rows, count]);
});

test('an answer that may hold `__proto__` keeps it as a key, in objects with no prototype', (t) => {
  const client = open(t);
  // A Map, as an object literal's `__proto__` would set its prototype rather than hold the key
  const meta = new Map([['__proto__', 1], ['other', 2]]);
  client.write({ ops: [{ op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one', meta } }] });
  const [visited] = client.read({ reads: [{ read: 'query', visits: true, plan: { start: notes, terminal: { answer: 'rows' } } }] });
  // The answer a `visits` wraps says it isn't plain, and the whole call is decoded exactly
  assert.ok(visited.kind === 'visited' && visited.answer.kind === 'rows', String(visited.kind));
  assert.equal(Object.getPrototypeOf(visited), null);
  const { rows, plainObjects } = visited.answer;
  assert.equal(plainObjects, false);
  const held = /** @type {Record<string, unknown>} */ (rows[0].meta);
  assert.equal(Object.getPrototypeOf(held), null);
  assert.deepEqual(Object.keys(held), ['__proto__', 'other']);
  assert.equal(held['__proto__'], 1);
  assert.equal(rows[0].title, 'one');
  // A field's value says it as rows do, and an `explain` is decoded through the answer it wraps
  const [field, explained] = client.read({ reads: [
    { read: 'field', id: 'Note-1', field: 'meta' },
    { read: 'query', explain: true, plan: { start: notes, terminal: { answer: 'firstRow', fields: ['meta'] } } },
  ] });
  assert.ok(field.kind === 'field' && explained.kind === 'explained' && explained.answer.kind === 'row');
  assert.equal(field.plainObjects, false);
  assert.deepEqual(Object.keys(/** @type {object} */ (field.value)), ['__proto__', 'other']);
  assert.deepEqual(Object.keys(/** @type {object} */ (explained.answer.row?.meta)), ['__proto__', 'other']);
});

test('a unit writes, reads its own writes, and commits through the client', (t) => {
  const client = open(t);
  const unit = client.begin();
  const result = unit.write({ ops: [{ op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one' } }] });
  assert.deepEqual(result.results, [{ kind: 'entity', id: 'Note-1', shortCode: null, created: true }]);
  const [count] = unit.read({ reads: [{ read: 'query', plan: { start: notes, terminal: { answer: 'count' } } }] });
  assert.deepEqual(count, { kind: 'count', n: 1 });
  assert.deepEqual(unit.pendingChanges().types, ['Note']);
  const committed = unit.commit();
  assert.deepEqual(Object.keys(committed.entities), ['Note-1']);
  assert.deepEqual(client.read({ reads: [{ read: 'row', id: 'Note-9' }] }), [{ kind: 'row', row: null, plainObjects: true }]);
});
