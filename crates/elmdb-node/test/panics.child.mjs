// The child of panics.test.mjs: started with ELMDB_TEST_PANICS set, so each export panics the first time it is
// called. It calls every export the binding declares, expects the panic as an error it can catch, calls it again,
// and says so on its last line.
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const { Database, Unit } = createRequire(import.meta.url)('../index.js');
const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });
const schema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  types: { Note: { fields: { title: { type: 'string' } } } },
};
const dir = mkdtempSync(join(tmpdir(), 'elmdb-panics-'));
const batch = (id) => packr.pack({ ops: [{ op: 'create', type: 'Note', id, fields: { title: id } }], now: 1000 });
const reads = packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'ids' } } }] });

/** The error `call` throws, or none */
const thrown = (call) => {
  try {
    call();
  } catch (error) {
    return error;
  }
  return undefined;
};

/** `call` panics the first time, as an error naming the export, and not the second */
function panicsOnce(name, call) {
  const first = thrown(call);
  assert.equal(first?.code, 'PANICKED', `${name}: ${first}`);
  assert.match(first.message, new RegExp(`^${name.replace('.', '\\.')} panicked at .*panics\\.rs:\\d+:\\d+: a panic a test asked for`));
  const second = thrown(call);
  assert.notEqual(second?.code, 'PANICKED', `${name}, called again: ${second}`);
}

/** Every member a class declares has a call here, so a new export is one this test is made to cover */
function covered(what, prototype, calls) {
  const declared = Object.getOwnPropertyNames(prototype).filter((name) => name !== 'constructor').sort();
  assert.deepEqual(Object.keys(calls).sort(), declared, `${what}'s exports`);
}

let db;
panicsOnce('Database.open', () => { db = Database.open(dir, { schema, syncs: false }); });

// A unit's exports first, on a unit begun once `begin` has panicked
const none = Buffer.alloc(0);
let unit;
panicsOnce('Database.begin', () => { unit = db.begin(); });
const ofUnit = {
  write: () => unit.write(batch('Note-unit')),
  setCheckpoint: () => unit.setCheckpoint('reader', Buffer.from('at')),
  checkpoint: () => unit.checkpoint('reader'),
  pendingChanges: () => unit.pendingChanges(),
  changedSince: () => unit.changedSince(['Note'], none, 10),
  nextSequence: () => unit.nextSequence(),
  read: () => unit.read(reads),
  savepoint: () => unit.savepoint(),
  rollbackTo: () => unit.rollbackTo(1),
  release: () => unit.release(1),
  isOpen: () => unit.isOpen,
  commit: () => unit.commit(),
  abort: () => unit.abort(),
};
covered('Unit', Unit.prototype, ofUnit);
for (const [name, call] of Object.entries(ofUnit)) panicsOnce(`Unit.${name}`, call);
assert.equal(unit.isOpen, false, 'the unit committed once its commit had panicked');

const ofDatabase = {
  path: () => db.path,
  readOnly: () => db.readOnly,
  isOpen: () => db.isOpen,
  schema: () => db.schema,
  setSchema: () => db.setSchema(schema),
  openReport: () => db.openReport,
  syncs: () => db.syncs,
  write: () => db.write(batch(`Note-${Math.random()}`)),
  readSync: () => db.readSync(reads),
  untrack: () => db.untrack(1),
  hearBuilderCommits: () => db.hearBuilderCommits(() => {}),
  hearCommitsElsewhere: () => db.hearCommitsElsewhere(() => {}),
  replacedElsewhere: () => db.replacedElsewhere(),
  adoptReplacement: () => db.adoptReplacement(0),
  trackStats: () => db.trackStats(),
  sequence: () => db.sequence,
  check: () => db.check(),
  rebuild: () => db.rebuild(),
  indexStatus: () => db.indexStatus(),
  committedElsewhere: () => db.committedElsewhere(),
  checkpoint: () => db.checkpoint('reader'),
  changedSince: () => db.changedSince(['Note'], none, 10),
  waitForIndexBuilds: () => db.waitForIndexBuilds(),
  purge: () => db.purge({ type: 'Gone' }),
  renameType: () => db.renameType('Gone', 'Went'),
  renameField: () => db.renameField('Note', 'gone', 'went'),
  renameRelation: () => db.renameRelation('gone', 'went'),
  backup: () => db.backup(join(dir, 'backup')),
  replaceWith: () => db.replaceWith(join(dir, 'nothing')),
  stats: () => db.stats(),
  bulkLoad: () => db.bulkLoad(packr.pack([])),
  reset: () => db.reset(),
};
// Those that answer a promise, and the two called before and after the rest
const apart = { begin: true, read: true, durable: true, close: true };
covered('Database', Database.prototype, { ...ofDatabase, ...apart });
for (const [name, call] of Object.entries(ofDatabase)) panicsOnce(`Database.${name}`, call);

// A promise's work is done on a thread of the pool: the call panics once on this thread, then its work once there,
// which rejects the promise, and then it answers
for (const [name, call] of [['read', () => db.read(reads)], ['durable', () => db.durable()]]) {
  assert.equal(thrown(call)?.code, 'PANICKED', name);
  const rejected = await call().then(() => undefined, (error) => error);
  assert.equal(rejected?.code, 'PANICKED', `${name} on the pool: ${rejected}`);
  assert.match(rejected.message, new RegExp(`^Database\\.${name}, on a thread of the pool, panicked at `));
  await call();
}

// The database is whole: a write lands, a read answers it, and a check finds nothing
db.write(batch('Note-after'));
const [answer] = packr.unpack(db.readSync(reads));
assert.ok(answer.ids.includes('Note-after'));
assert.ok(db.check().differences.length === 0);
panicsOnce('Database.close', () => db.close());
assert.equal(db.isOpen, false);
rmSync(dir, { recursive: true, force: true });
console.log('every export panicked once, and the process lives');
