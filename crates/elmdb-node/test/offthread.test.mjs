// Write, check, rebuild and backup run on a thread of the pool, answering a promise: a large rebuild doesn't hold the
// event loop. A database's writes and rebuilds are applied in the order they were asked for, by one job of the pool at
// a time. Their `…Sync` forms are the blocking ones. A failure reaches JavaScript as the promise's rejection with its
// code; a refusal made before any work starts throws at the call, as it did.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { stat } from 'node:fs/promises';
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

test('a write runs off the thread that asked: an event-loop turn comes before its answer', async (t) => {
  const { db } = open(t);
  // A batch that takes longer than a turn of the loop. Its answer can't come before a turn that was asked for first
  // unless the write ran on another thread: a blocking write answers before any turn is taken
  const ops = Array.from({ length: 5000 }, (_, i) => ({ op: 'create', type: 'Note', id: `Note-${i}`, fields: { title: `t ${i}` } }));
  const order = [];
  const pending = db.write(packr.pack({ ops, now: 1000 })).then(() => order.push('answer'));
  setImmediate(() => order.push('turn'));
  await pending;
  assert.deepEqual(order, ['turn', 'answer']);
});

test('a rebuild of a large database leaves the event loop turning until it answers', async (t) => {
  const { db } = open(t);
  fill(db, 20000);
  let settled = false;
  const rebuilding = db.rebuild().then(() => { settled = true; });
  // Timers keep firing while the rebuild runs: the loop turns many times before the rebuild's answer
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
  const { db } = open(t);
  fill(db, 500);
  assert.deepEqual(await db.check(), db.checkSync());
  assert.equal(db.checkSync().violations.length, 0);
  await db.rebuild('Note', 'byTitle');
  // A directory of its own for the copy: splitting the database's path would not hold on Windows' separators
  const copies = mkdtempSync(join(tmpdir(), 'elmdb-offthread-copy-'));
  t.after(() => rmSync(copies, { recursive: true, force: true }));
  const target = join(copies, 'copy');
  await db.backup(target);
  // Closed before the directory is removed: Windows won't remove a file that is open
  const copy = Database.open(target, { schema, syncs: false });
  let notes;
  try {
    notes = packr.unpack(copy.readSync(count))[0].n;
  } finally {
    copy.close();
  }
  assert.equal(notes, 500);
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

test('writes in flight at once are applied in the order they were asked for', async (t) => {
  const { db } = open(t);
  db.writeSync(create('Note-0', 'made'));
  const before = packr.unpack(db.writeSync(create('Note-1', 'made'))).sequence;
  // Sixty writes of one field, none awaited before the next is asked for: each commits straight after the one asked
  // before it, and the last one asked for is what the note holds
  const titled = (i) => packr.pack({ ops: [{ op: 'set', id: 'Note-0', fields: { title: `title ${i}` } }], now: 1000 });
  const asked = Array.from({ length: 60 }, (_, i) => db.write(titled(i)));
  const sequences = (await Promise.all(asked)).map((answer) => packr.unpack(answer).sequence);
  assert.deepEqual(sequences, Array.from({ length: 60 }, (_, i) => before + 1 + i));
  const read = packr.pack({ reads: [{ read: 'row', id: 'Note-0' }] });
  assert.equal(packr.unpack(db.readSync(read))[0].row.title, 'title 59');
});

test('writes waiting their turn behind a rebuild hold one thread of the pool between them', async (t) => {
  const { db, dir } = open(t);
  fill(db, 20000);
  // A rebuild, and behind it three times as many writes as the pool has threads: each waits for the rebuild to end
  const threads = Number(process.env.UV_THREADPOOL_SIZE) || 4;
  let rebuilt = false;
  const began = performance.now();
  const rebuilding = db.rebuild().then(() => { rebuilt = performance.now() - began; });
  const writes = Array.from({ length: 3 * threads }, (_, i) => db.write(create(`Note-Late-${i}`, `late ${i}`)));
  // What Node asks of the pool is answered while they wait: each a job of its own, the writes would hold every thread
  // until the rebuild ended, and this would wait behind them
  await stat(dir);
  const answered = performance.now() - began;
  assert.equal(rebuilt, false, `the pool answered ${answered.toFixed(1)} ms in, after the rebuild ended at ${rebuilt} ms`);
  await Promise.all([rebuilding, ...writes]);
  assert.equal(packr.unpack(db.readSync(count))[0].n, 20000 + 3 * threads);
});

test('a write and a rebuild, in flight together, both land and the database checks clean', async (t) => {
  const { db } = open(t);
  fill(db, 2000);
  const writes = [];
  for (let i = 0; i < 20; i++) writes.push(db.write(create(`Note-Late-${i}`, `late ${i}`)));
  await Promise.all([db.rebuild(), ...writes]);
  assert.equal(packr.unpack(db.readSync(count))[0].n, 2020);
  assert.equal(db.checkSync().differences.length, 0);
});
