// Write, check, rebuild and backup run on a thread of the pool, answering a promise: a large rebuild doesn't hold the
// event loop. Their `…Sync` forms are the blocking ones. A failure reaches JavaScript as the promise's rejection with
// its code; a refusal made before any work starts throws at the call, as it did.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const { Database } = createRequire(import.meta.url)('../index.js');
const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });
const schema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  types: { Note: { fields: { title: { type: 'string' }, n: { type: 'number', optional: true } }, indexes: { byTitle: { kind: 'value', fields: ['title'] } } } },
};

function open(t) {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-offthread-'));
  const db = Database.open(dir, { schema, syncs: false });
  t.after(() => {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  });
  return { db, dir };
}

const create = (id, title) => packr.pack({ ops: [{ op: 'create', type: 'Note', id, fields: { title } }], now: 1000 });
const count = packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'count' } } }] });

/** Fills the database with `total` notes, a chunk of them per write, each title held by one note */
function fill(db, total) {
  for (let from = 0; from < total; from += 2000) {
    const ops = [];
    for (let i = from; i < Math.min(total, from + 2000); i++) {
      ops.push({ op: 'create', type: 'Note', id: `Note-${i}`, fields: { title: `title ${i}`, n: i } });
    }
    db.writeSync(packr.pack({ ops, now: 1000 }));
  }
}

test('a write answers a promise of what the blocking write answers, and the write is there', async (t) => {
  const { db } = open(t);
  const answer = await db.write(create('Note-1', 'one'));
  assert.ok(Buffer.isBuffer(answer));
  const blocking = packr.unpack(db.writeSync(create('Note-2', 'two')));
  assert.equal(blocking.results[0].kind, 'entity');
  assert.equal(packr.unpack(await db.write(create('Note-3', 'three'))).results[0].id, 'Note-3');
  assert.equal(packr.unpack(db.readSync(count))[0].n, 3);
});

test('a write runs off the thread that asked: the call returns before it is applied', async (t) => {
  const { db } = open(t);
  let applied = false;
  const pending = db.write(create('Note-1', 'one')).then(() => { applied = true; });
  assert.equal(applied, false, 'the call returned before the batch was applied');
  await pending;
  assert.equal(applied, true);
});

test('a rebuild of a large database leaves the event loop turning until it answers', async (t) => {
  const { db } = open(t);
  fill(db, 20000);
  let settled = false;
  const rebuilding = db.rebuild().then(() => { settled = true; });
  // The call returned before the rebuild began to be answered, and timers keep firing while it runs
  assert.equal(settled, false);
  let turns = 0;
  while (!settled) {
    await new Promise((resolve) => setImmediate(resolve));
    turns++;
  }
  await rebuilding;
  assert.ok(turns > 1, `the event loop turned ${turns} times while the rebuild ran`);
  assert.equal(db.checkSync().differences.length, 0);
});

test('a check, a backup and a rebuild answer what their blocking forms answer', async (t) => {
  const { db, dir } = open(t);
  fill(db, 500);
  assert.deepEqual(await db.check(), db.checkSync());
  assert.equal(db.checkSync().violations.length, 0);
  await db.rebuild('Note', 'byTitle');
  const target = join(dir, '..', `${dir.split('/').pop()}-backup`);
  t.after(() => rmSync(target, { recursive: true, force: true }));
  await db.backup(target);
  const copy = Database.open(target, { schema, syncs: false });
  t.after(() => copy.close());
  assert.equal(packr.unpack(copy.readSync(count))[0].n, 500);
});

test('a failure is the promise rejecting with elmdb\'s code, and a refusal made before the work throws', async (t) => {
  const { db } = open(t);
  // A write refused by the batch itself
  await assert.rejects(db.write(packr.pack({ ops: [{ op: 'explode' }] })), { code: 'BATCH_INVALID' });
  // A rebuild of an index the schema doesn't declare
  await assert.rejects(db.rebuild('Note', 'nope'), { code: 'UNKNOWN_INDEX' });
  // A rebuild naming only a type is refused where it is called, with no work begun
  assert.throws(() => db.rebuild('Note'), { code: 'BATCH_INVALID' });
  // A write while a unit is open is refused, and so is any database write made beside it
  const unit = db.begin();
  await assert.rejects(db.write(create('Note-1', 'one')), { code: 'UNIT_OPEN' });
  unit.abort();
  // Its own write goes through once the unit has ended
  assert.ok(Buffer.isBuffer(await db.write(create('Note-2', 'two'))));
});

test('a write and a rebuild on the pool, in flight together, both land and the database checks clean', async (t) => {
  const { db } = open(t);
  fill(db, 2000);
  const writes = [];
  for (let i = 0; i < 20; i++) writes.push(db.write(create(`Note-Late-${i}`, `late ${i}`)));
  await Promise.all([db.rebuild(), ...writes]);
  assert.equal(packr.unpack(db.readSync(count))[0].n, 2020);
  assert.equal(db.checkSync().differences.length, 0);
});
