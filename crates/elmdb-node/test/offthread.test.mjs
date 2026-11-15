// Check, rebuild and backup run on a thread of the pool, answering a promise: a large rebuild doesn't hold the event
// loop. Their `…Sync` forms are the blocking ones, and a write is always blocking. A rebuild takes the database's write
// lock as a write does, so a write made beside it lands before or after it, and either way the database checks clean.
// A failure reaches JavaScript as the promise's rejection with its code; a refusal made before any work starts throws
// at the call, as it did.
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

test('a rebuild of a large database leaves the event loop turning until it answers', async (t) => {
  const { db } = open(t);
  fill(db, 20000);
  // The loop keeps turning while the rebuild runs: the longest it goes without a turn is a small part of the rebuild,
  // where a rebuild that did its work on the loop's thread, at any point before its answer, would hold it for most of
  // it. Half the rebuild, or 50 ms where the rebuild is too short for half of it to tell a held loop from a busy machine
  const rebuild = async () => {
    let settled = false;
    const began = performance.now();
    const rebuilding = db.rebuild().then(() => { settled = true; });
    let [turns, last, longest] = [0, began, 0];
    while (!settled) {
      await new Promise((resolve) => setImmediate(resolve));
      const now = performance.now();
      longest = Math.max(longest, now - last);
      last = now;
      turns++;
    }
    await rebuilding;
    return { turns, longest, took: performance.now() - began };
  };
  // The best of three: a machine busy with the suite's other files can stall the loop once, and a rebuild that holds
  // the loop holds it every time
  const runs = [];
  for (let i = 0; i < 3; i++) {
    runs.push(await rebuild());
    const { turns, longest, took } = runs.at(-1);
    if (turns > 1 && longest < Math.max(took / 2, 50)) break;
  }
  const { turns, longest, took } = runs.at(-1);
  const told = runs.map((run) => `${run.longest.toFixed(1)} of ${run.took.toFixed(1)} ms in ${run.turns} turns`).join('; ');
  assert.ok(turns > 1 && longest < Math.max(took / 2, 50), `the event loop's longest wait for a turn, of each rebuild: ${told}`);
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
  // A rebuild of an index the schema doesn't declare
  await assert.rejects(db.rebuild('Note', 'nope'), { code: 'UNKNOWN_INDEX' });
  // A rebuild naming only a type is refused where it is called, with no work begun
  assert.throws(() => db.rebuild('Note'), { code: 'BATCH_INVALID' });
});

test('a rebuild while a unit is open is refused, and runs once the unit has ended', async (t) => {
  const { db } = open(t);
  fill(db, 500);
  // The unit holds the database's one write transaction: a rebuild on the pool waiting for it would wait on a unit
  // only this thread can end, so it is refused instead
  const unit = db.begin();
  unit.write(create('Note-unit', 'in the unit'));
  await assert.rejects(db.rebuild(), { code: 'UNIT_OPEN' });
  unit.commit();
  await db.rebuild();
  assert.equal(packr.unpack(db.readSync(count))[0].n, 501);
  assert.equal(db.checkSync().differences.length, 0);
});

test('writes made beside a rebuild on the pool land, whichever commits first, and the database checks clean', async (t) => {
  const { db } = open(t);
  fill(db, 20000);
  const before = db.sequence;
  // A rebuild of every index, and twenty blocking writes asked for while it is on its way: each waits for the rebuild
  // where it has the write lock, and commits ahead of it where it hasn't yet. The rebuild derives from what is
  // committed when it begins, and each write keeps the indexes as it commits, so no order leaves a difference
  const rebuilding = db.rebuild();
  const sequences = [];
  for (let i = 0; i < 20; i++) sequences.push(packr.unpack(db.writeSync(create(`Note-Late-${i}`, `late ${i}`))).sequence);
  await rebuilding;
  // Each write's commit is the next after the one before it, or after the rebuild's: twenty writes and one rebuild
  // made twenty-one commits between them
  assert.equal(db.sequence, before + 21);
  assert.ok(sequences.every((sequence, i) => i === 0 || sequence > sequences[i - 1]), `sequences ${sequences}`);
  // The rebuild's commit is the one of the twenty-one none of the writes took
  const rebuilt = Array.from({ length: 21 }, (_, i) => before + 1 + i).find((sequence) => !sequences.includes(sequence));
  t.diagnostic(`the rebuild committed ${rebuilt - before} of 21: ${rebuilt - before - 1} writes before it, ${before + 21 - rebuilt} after`);
  assert.equal(packr.unpack(db.readSync(count))[0].n, 20020);
  assert.equal(db.checkSync().differences.length, 0);
});

test('a check, a rebuild and a backup asked of a closed database reject with CLOSED', async (t) => {
  const { db } = open(t);
  fill(db, 50);
  db.close();
  await assert.rejects(db.check(), { code: 'CLOSED' });
  await assert.rejects(db.rebuild(), { code: 'CLOSED' });
  await assert.rejects(db.backup(join(tmpdir(), 'elmdb-offthread-never')), { code: 'CLOSED' });
});

test('a rebuild asked for as a close takes the database runs before it or rejects as closed, leaving it whole', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-offthread-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const db = Database.open(dir, { schema, syncs: false });
  fill(db, 2000);
  // Asked for and the database closed in the same turn: the close waits for the rebuild if it has begun, and a rebuild
  // that hasn't finds no database
  const rebuilt = db.rebuild();
  db.close();
  const [settled] = await Promise.allSettled([rebuilt]);
  if (settled.status === 'rejected') assert.equal(settled.reason.code, 'CLOSED');
  // Closed before the directory is removed: Windows won't remove a file that is open
  const again = Database.open(dir, { schema, syncs: false });
  let held;
  let differences;
  try {
    held = packr.unpack(again.readSync(count))[0].n;
    differences = again.checkSync().differences.length;
  } finally {
    again.close();
  }
  assert.equal(held, 2000);
  assert.equal(differences, 0, `the rebuild ${settled.status}`);
});
