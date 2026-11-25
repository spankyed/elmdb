// Every answer in bytes is a Buffer whose memory V8 allocated itself, never an external buffer over Rust's bytes. V8
// frees an external buffer's bytes on a thread of its collector, where Node wakes the environment to have the
// JavaScript thread run the finalizer; an environment that is ending closes what that wakes before it stops waking it,
// and libuv on Windows aborts the process on the wake (`exit.test.mjs` ends a process that way). Node marks the
// memory of an external buffer as never to be transferred to another thread, since its finalizer belongs to this one,
// and marks no memory V8 allocated: `isMarkedAsUntransferable` reads the mark. Which exports answer bytes is read
// from the generated declarations, so a method added later that answers a Buffer is held to this or fails here until
// it is. Bytes handed over any other way, inside an object or to a listener, the declarations don't tell apart.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { isMarkedAsUntransferable } from 'node:worker_threads';
import { Packr } from 'msgpackr';

const { Database } = createRequire(import.meta.url)('../index.js');
const packr = new Packr({ useRecords: false, mapsAsObjects: true });

/** Every method the declarations say answers bytes, as `Class.method`. A checkout on Windows ends lines in CRLF */
function answeringBytes() {
  const found = [];
  let within = null;
  for (const line of readFileSync(new URL('../index.d.ts', import.meta.url), 'utf8').split(/\r?\n/)) {
    within = line.match(/^export declare class (\w+)/)?.[1] ?? (line.startsWith('}') ? null : within);
    const method = line.match(/^\s+(\w+)\(.*\): (?:Uint8Array|Uint8Array \| null|Promise<Uint8Array>)$/)?.[1];
    if (within && method) found.push(`${within}.${method}`);
  }
  return found.sort();
}

const schema = { $schema: 'https://elmdb.dev/schema/1.json', relations: {}, types: { Note: { fields: {} } } };

test('every answer in bytes is memory V8 allocated, which freeing calls nothing for', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-answers-'));
  const db = Database.open(dir, { schema, syncs: false });
  t.after(() => { db.close(); rmSync(dir, { recursive: true, force: true }); });

  const create = (id) => packr.pack({ ops: [{ op: 'create', type: 'Note', id }], now: 1000 });
  const count = packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'count' } } }] });
  const since = packr.pack({ sequence: 0 });

  const answered = [];
  const owned = (name, answer) => {
    assert.ok(Buffer.isBuffer(answer) && answer.length > 0, `${name} answered no bytes`);
    assert.equal(isMarkedAsUntransferable(answer.buffer), false, `${name} answered an external buffer`);
    answered.push(name);
  };

  owned('Database.write', db.write(create('Note-1')));
  owned('Database.read', db.read(count));
  owned('Database.readAsync', await db.readAsync(count));
  owned('Database.changedSince', db.changedSince(['Note'], since, 100));
  const unit = db.begin();
  owned('Unit.write', unit.write(create('Note-2')));
  unit.setCheckpoint('reader', Buffer.from('there'));
  owned('Unit.checkpoint', unit.checkpoint('reader'));
  owned('Unit.pendingChanges', unit.pendingChanges());
  owned('Unit.changedSince', unit.changedSince(['Note'], since, 100));
  owned('Unit.read', unit.read(count));
  owned('Unit.commit', unit.commit());
  owned('Database.checkpoint', db.checkpoint('reader'));
  assert.deepEqual(answered.sort(), answeringBytes(), 'an export that answers bytes is held to none of this');
  assert.equal(answered.length, 11);
});

test('every method taking bytes accepts a plain Uint8Array as well as a Buffer', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-u8-'));
  const db = Database.open(dir, { schema, syncs: false });
  t.after(() => { db.close(); rmSync(dir, { recursive: true, force: true }); });

  const u8 = (value) => {
    const buf = packr.pack(value);
    const bytes = new Uint8Array(buf.byteLength);
    bytes.set(buf);
    assert.equal(Buffer.isBuffer(bytes), false);
    return bytes;
  };
  const create = (id) => u8({ ops: [{ op: 'create', type: 'Note', id }], now: 1000 });
  const count = u8({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'count' } } }] });
  const since = u8({ sequence: 0 });
  const checkpoint = new Uint8Array([1, 2, 3]);

  db.write(create('Note-1'));
  assert.equal(packr.unpack(db.read(count))[0].n, 1);
  assert.equal(packr.unpack(await db.readAsync(count))[0].n, 1);
  assert.ok(packr.unpack(db.changedSince(['Note'], since, 100)));
  db.bulkLoad(u8([{ load: 'entity', id: 'Note-2', fields: {}, trashed: false }]));

  const unit = db.begin();
  unit.write(create('Note-3'));
  assert.equal(packr.unpack(unit.read(count))[0].n, 3);
  assert.ok(packr.unpack(unit.changedSince(['Note'], since, 100)));
  unit.setCheckpoint('u8', checkpoint);
  assert.deepEqual(new Uint8Array(unit.checkpoint('u8')), checkpoint);
  unit.commit();
  assert.deepEqual(new Uint8Array(db.checkpoint('u8')), checkpoint);
});
