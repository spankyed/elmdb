import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const { Database } = createRequire(import.meta.url)('../index.js');
// apack's codec: maps decoded as maps, so a field named __proto__ is a field
const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });

const schema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  relations: { refs: { cardinality: 'many-to-many' } },
  types: { Note: { shortCode: { prefix: 'NTE' }, fields: { title: { type: 'string' }, at: { type: 'any', optional: true } } } },
};

function open(t) {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  const db = Database.open(dir, { schema, syncs: false });
  t.after(() => { db.close(); rmSync(dir, { recursive: true, force: true }); });
  return db;
}

const write = (target, ops) => packr.unpack(target.write(packr.pack({ ops, now: 1000 })));

test('a batch creates and links, and answers each operation and the change set', (t) => {
  const db = open(t);
  const answer = write(db, [
    { op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one', at: new Date(5) } },
    { op: 'create', type: 'Note', id: 'Note-2', fields: { title: 'two' } },
    { op: 'link', source: 'Note-1', kind: 'refs', target: 'Note-2' },
  ]);
  assert.deepEqual(answer.results[0], { kind: 'entity', id: 'Note-1', shortCode: 'NTE-1', created: true });
  assert.equal(answer.results[2].kind, 'relation');
  assert.deepEqual(answer.changes.types, ['Note']);
  assert.equal(answer.changes.entities['Note-1'].created, true);
});

test('a refused batch throws its code and writes nothing', (t) => {
  const db = open(t);
  assert.throws(() => db.write(packr.pack({ ops: [{ op: 'create', type: 'Note', fields: {} }] })), { code: 'REQUIRED_FIELD' });
  // Bytes outside msgpack's value spec are a value elmdb doesn't store; well-formed bytes that are no batch are a batch
  assert.throws(() => db.write(Buffer.from([0xc1])), { code: 'VALUE_INVALID' });
  assert.throws(() => db.write(packr.pack({ ops: [{ op: 'set', id: 'Note-1', fields: { n: Number.NaN }, touch: false }] })), { code: 'VALUE_INVALID' });
  assert.throws(() => db.write(packr.pack({ ops: [{ op: 'explode' }] })), { code: 'BATCH_INVALID' });
});

test('a unit reads its own writes, refuses a write beside it, and commits its change set', (t) => {
  const db = open(t);
  const unit = db.begin();
  write(unit, [{ op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one' } }]);
  assert.throws(() => db.write(packr.pack({ ops: [] })), { code: 'UNIT_OPEN' });
  const depth = unit.savepoint();
  write(unit, [{ op: 'create', type: 'Note', id: 'Note-2', fields: { title: 'two' } }]);
  unit.rollbackTo(depth);
  const changes = packr.unpack(unit.commit());
  assert.deepEqual(Object.keys(changes.entities), ['Note-1']);
  assert.equal(unit.isOpen, false);
  assert.throws(() => unit.commit(), { code: 'CLOSED' });
  write(db, [{ op: 'set', id: 'Note-1', fields: { title: 'after' } }]);
});

test('reads answer on this thread and on the pool, the same, and a unit reads its own writes', async (t) => {
  const db = open(t);
  write(db, [
    { op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one' } },
    { op: 'create', type: 'Note', id: 'Note-2', fields: { title: 'two' } },
  ]);
  const reads = packr.pack({ reads: [
    { read: 'query', plan: { start: { from: 'types', types: ['Note'] }, steps: [{ step: 'orderBy', field: 'title', descending: true }], terminal: { answer: 'rows', fields: ['title'] } } },
    { read: 'row', id: 'Note-1' },
  ] });
  const now = packr.unpack(db.readSync(reads));
  const pooled = packr.unpack(await db.read(reads));
  assert.deepEqual(pooled, now);
  assert.deepEqual(now[0].rows, [{ id: 'Note-2', title: 'two' }, { id: 'Note-1', title: 'one' }]);
  assert.equal(now[0].plainObjects, true);
  assert.equal(now[1].row.entityType, 'Note');
  await assert.rejects(db.read(packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Nope'] }, terminal: { answer: 'ids' } } }] })), { code: 'UNKNOWN_TYPE' });
  const unit = db.begin();
  write(unit, [{ op: 'create', type: 'Note', id: 'Note-3', fields: { title: 'three' } }]);
  const inUnit = packr.unpack(unit.read(packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'count' } } }] })));
  assert.equal(inUnit[0].n, 3);
  unit.abort();
});

test('admin: stats, check, a backup that opens on its own, reset, and a load the checks would refuse', (t) => {
  const db = open(t);
  write(db, [{ op: 'create', type: 'Note', id: 'Note-1', fields: { title: 'one' } }]);
  assert.equal(db.stats().types.Note.live, 1);
  assert.deepEqual(db.check(), { differences: [], violations: [] });
  assert.ok(db.indexStatus().every((s) => s.built));
  const target = mkdtempSync(join(tmpdir(), 'elmdb-backup-'));
  t.after(() => rmSync(target, { recursive: true, force: true }));
  db.backup(join(target, 'b'));
  const copy = Database.open(join(target, 'b'), { readOnly: true });
  assert.equal(packr.unpack(copy.readSync(packr.pack({ reads: [{ read: 'row', id: 'Note-1' }] })))[0].row.title, 'one');
  copy.close();
  db.bulkLoad(packr.pack([{ load: 'entity', id: 'Note-2', fields: {} }]));
  assert.match(db.check().violations[0].message, /lacks title/);
  assert.throws(() => db.purge({ type: 'Note' }), { code: 'SCHEMA_CHANGE_REFUSED' });
  db.reset();
  assert.equal(db.stats().types.Note, undefined);
});
