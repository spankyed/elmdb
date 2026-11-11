// Every answer in bytes is a Buffer whose memory V8 allocated itself, never an external buffer over Rust's bytes. V8
// frees an external buffer's bytes on a thread of its collector, where Node wakes the environment to have the
// JavaScript thread run the finalizer; an environment that is ending closes what that wakes before it stops waking it,
// and libuv on Windows aborts the process on the wake (`exit.test.mjs` ends a process that way). Node counts the bytes
// its allocator hands V8 in `process.memoryUsage().arrayBuffers`, and counts none of an external buffer's.
//
// This file holds the one test, so nothing else in its process allocates or frees a buffer between two readings.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const { Database } = createRequire(import.meta.url)('../index.js');
const packr = new Packr({ useRecords: false, mapsAsObjects: true });

const schema = { $schema: 'https://elmdb.dev/schema/1.json', relations: {}, types: { Note: { fields: {} } } };

// Electron's V8 allows no external buffer at all, so N-API copies every answer there whatever the binding asks for,
// and its allocator isn't the one Node counts
const electron = process.versions.electron && 'Electron allows no external buffer, and Node counts none of its allocator\'s bytes';

test('every answer in bytes is memory V8 allocated, which freeing calls nothing for', { skip: electron }, async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-answers-'));
  const db = Database.open(dir, { schema, syncs: false });
  t.after(() => { db.close(); rmSync(dir, { recursive: true, force: true }); });

  // Every request is made before the first reading, so a reading's growth is its answer's alone
  const create = (id) => packr.pack({ ops: [{ op: 'create', type: 'Note', id }], now: 1000 });
  const [one, two] = [create('Note-1'), create('Note-2')];
  const count = packr.pack({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'count' } } }] });
  const since = packr.pack({ sequence: 0 });
  const mark = Buffer.from('there');

  // Held to the end, so no answer is freed while another is read
  const answers = [];
  const owned = async (name, call) => {
    const before = process.memoryUsage().arrayBuffers;
    const answer = await call();
    const grew = process.memoryUsage().arrayBuffers - before;
    answers.push(answer);
    assert.ok(answer.length > 0, `${name} answered no bytes`);
    assert.ok(grew >= answer.length, `${name} answered ${answer.length} bytes, and V8's own buffers grew by ${grew}`);
  };

  await owned('Database.write', () => db.write(one));
  await owned('Database.readSync', () => db.readSync(count));
  await owned('Database.read', () => db.read(count));
  await owned('Database.changedSince', () => db.changedSince(['Note'], since, 100));
  const unit = db.begin();
  await owned('Unit.write', () => unit.write(two));
  unit.setCheckpoint('reader', mark);
  await owned('Unit.checkpoint', () => unit.checkpoint('reader'));
  await owned('Unit.pendingChanges', () => unit.pendingChanges());
  await owned('Unit.changedSince', () => unit.changedSince(['Note'], since, 100));
  await owned('Unit.read', () => unit.read(count));
  await owned('Unit.commit', () => unit.commit());
  await owned('Database.checkpoint', () => db.checkpoint('reader'));
  assert.equal(answers.length, 11);
});
