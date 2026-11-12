// The child of panics.test.mjs: `node panics.child.mjs <dir> <export>`, started with ELMDB_TEST_PANIC naming the
// export, which then panics the first time it is called. The child calls it on the database at <dir>, expects the
// panic as an error it can catch, finds the handle refusing calls after it, closes it, and opens the database again.
// Its last line says so.
import assert from 'node:assert/strict';
import { basename, join } from 'node:path';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const { Database } = createRequire(import.meta.url)('../index.js');
const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });
const [dir, name] = process.argv.slice(2);
export const schema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  types: { Note: { fields: { title: { type: 'string' } } } },
};
const batch = (id) => packr.pack({ ops: [{ op: 'create', type: 'Note', id, fields: { title: id } }], now: 1000 });
const reads = packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'ids' } } }] });
const none = Buffer.alloc(0);
const open = () => Database.open(dir, { schema, syncs: false });
const ids = (db) => packr.unpack(db.readSync(reads))[0].ids;

/** The error `call` throws, or none */
const thrown = (call) => {
  try {
    call();
  } catch (error) {
    return error;
  }
  return undefined;
};

/** The panic the export raised: an error naming the export and where it was raised */
function panicked(error) {
  assert.equal(error?.code, 'PANICKED', `${name}: ${error}`);
  assert.match(error.message, new RegExp(`^${name.replaceAll('.', '\\.')} panicked at .*panics\\.rs:\\d+:\\d+: a panic a test asked for\\. `));
}

/** A handle that panicked refuses a call that asks something of the engine, saying what to do and what the panic was */
function refuses(db) {
  const refused = thrown(() => db.stats());
  assert.equal(refused?.code, 'PANICKED', `after ${name}: ${refused}`);
  assert.match(refused.message, /^Database\.stats is refused: this database panicked earlier.*Close it and open it again/);
  assert.ok(refused.message.includes(`${name} panicked at `), refused.message);
}

const ofDatabase = {
  path: (db) => db.path,
  readOnly: (db) => db.readOnly,
  isOpen: (db) => db.isOpen,
  schema: (db) => db.schema,
  setSchema: (db) => db.setSchema(schema),
  openReport: (db) => db.openReport,
  syncs: (db) => db.syncs,
  write: (db) => db.write(batch('Note-lost')),
  readSync: (db) => db.readSync(reads),
  read: (db) => db.read(reads),
  untrack: (db) => db.untrack(1),
  hearBuilderCommits: (db) => db.hearBuilderCommits(() => {}),
  hearCommitsElsewhere: (db) => db.hearCommitsElsewhere(() => {}),
  replacedElsewhere: (db) => db.replacedElsewhere(),
  adoptReplacement: (db) => db.adoptReplacement(0),
  trackStats: (db) => db.trackStats(),
  sequence: (db) => db.sequence,
  begin: (db) => db.begin(),
  check: (db) => db.check(),
  rebuild: (db) => db.rebuild(),
  indexStatus: (db) => db.indexStatus(),
  committedElsewhere: (db) => db.committedElsewhere(),
  checkpoint: (db) => db.checkpoint('reader'),
  changedSince: (db) => db.changedSince(['Note'], none, 10),
  waitForIndexBuilds: (db) => db.waitForIndexBuilds(),
  purge: (db) => db.purge({ type: 'Gone' }),
  renameType: (db) => db.renameType('Gone', 'Went'),
  renameField: (db) => db.renameField('Note', 'gone', 'went'),
  renameRelation: (db) => db.renameRelation('gone', 'went'),
  backup: (db) => db.backup(join(dir, 'backup')),
  replaceWith: (db) => db.replaceWith(join(dir, 'nothing')),
  stats: (db) => db.stats(),
  bulkLoad: (db) => db.bulkLoad(packr.pack([])),
  reset: (db) => db.reset(),
  durable: (db) => db.durable(),
  close: (db) => db.close(),
};
const ofUnit = {
  write: (unit) => unit.write(batch('Note-unit')),
  setCheckpoint: (unit) => unit.setCheckpoint('reader', Buffer.from('at')),
  checkpoint: (unit) => unit.checkpoint('reader'),
  pendingChanges: (unit) => unit.pendingChanges(),
  changedSince: (unit) => unit.changedSince(['Note'], none, 10),
  nextSequence: (unit) => unit.nextSequence(),
  read: (unit) => unit.read(reads),
  savepoint: (unit) => unit.savepoint(),
  rollbackTo: (unit) => unit.rollbackTo(1),
  release: (unit) => unit.release(1),
  commit: (unit) => unit.commit(),
  abort: (unit) => unit.abort(),
  isOpen: (unit) => unit.isOpen,
};

const [of, member, pool] = name.match(/^(Database|Unit)\.(\w+)(, on a thread of the pool,)?$/).slice(1);
let db;
if (name === 'Database.open') {
  // No handle is made by an open that panics
  panicked(thrown(open));
} else if (of === 'Database') {
  db = open();
  const call = ofDatabase[member];
  assert.ok(call, `${name} has no call in this test`);
  // A unit open as the database's call panics, which the panic ends: no call is made on it
  const unit = member === 'begin' ? undefined : db.begin();
  unit?.write(batch('Note-in-the-unit'));
  if (pool) {
    // The work of a promise is done on a thread of the pool, where its panic rejects the promise
    panicked(await call(db).then(() => undefined, (error) => error));
    // And the next call is refused where it is made, before any promise
    assert.match(thrown(() => call(db))?.message ?? '', /^Database\.\w+ is refused/);
  } else {
    panicked(thrown(() => call(db)));
  }
  refuses(db);
  // Its path and whether it is open are still answered, and it closes, its unit having ended
  assert.ok(db.path.endsWith(basename(dir)), db.path);
  assert.equal(db.isOpen, true);
  db.close();
  assert.equal(db.isOpen, false);
  assert.equal(unit?.isOpen ?? false, false);
} else {
  db = open();
  const unit = db.begin();
  const call = ofUnit[member];
  assert.ok(call, `${name} has no call in this test`);
  // Something for the unit to lose, where its own write is not the call that panics
  if (member !== 'write') unit.write(batch('Note-in-the-unit'));
  panicked(thrown(() => call(unit)));
  // The panic ended the unit, and a call on it is refused as any of its handle is
  assert.equal(unit.isOpen, false);
  assert.equal(thrown(() => unit.commit())?.code, 'PANICKED');
  refuses(db);
  // The close is not refused for an open unit
  db.close();
}

// The database is whole: it opens, none of a unit's writes or of a write that panicked is there, a write lands and
// a read answers it, and a check finds nothing
db = open();
assert.deepEqual(ids(db), ['Note-kept']);
const unit = db.begin();
unit.write(batch('Note-after'));
assert.deepEqual(packr.unpack(unit.read(reads))[0].ids, ['Note-kept', 'Note-after']);
unit.abort();
assert.equal(db.check().differences.length, 0);
db.close();
console.log(`${name} panicked, and the process lives`);
