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
  types: {
    Message: {
      fields: { text: { type: 'any', optional: true }, at: { type: 'any', optional: true } },
      indexes: { text: { kind: 'text', fields: { text: {} } } },
    },
  },
};

test('a search crosses the binding: hits with highlights, a suggestion and a quick-open', (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-'));
  const db = Database.open(dir, { schema, syncs: false });
  t.after(() => { db.close(); rmSync(dir, { recursive: true, force: true }); });
  db.write(packr.pack({ ops: [
    { op: 'create', type: 'Message', id: 'Message-1', fields: { text: 'deploying the database', at: new Date(1000) } },
    { op: 'create', type: 'Message', id: 'Message-2', fields: { text: 'Database backups', at: new Date(90_000) } },
  ], now: 1000 }));
  const [hits, newest, suggestions, labels] = packr.unpack(db.readSync(packr.pack({ reads: [
    { read: 'query', plan: {
      start: { from: 'types', types: ['Message'] },
      steps: [{ step: 'search', query: 'databse', options: { prefix: true } }],
      terminal: { answer: 'hits', highlights: true },
    } },
    { read: 'query', plan: {
      start: { from: 'types', types: ['Message'] },
      steps: [{ step: 'search', query: 'database', options: { recency: { field: 'at', halfLife: 10_000, now: 100_000 } } }],
      terminal: { answer: 'ids' },
    } },
    { read: 'suggest', type: 'Message', term: 'dep' },
    { read: 'findLabels', query: 'dbk', fields: { Message: 'text' } },
  ] })));
  assert.deepEqual(hits.hits.map((h) => h.id).sort(), ['Message-1', 'Message-2']);
  assert.deepEqual(hits.hits.find((h) => h.id === 'Message-1').highlights, { text: [[0, 14, 22]] });
  assert.deepEqual(newest.ids, ['Message-2', 'Message-1']);
  assert.deepEqual(suggestions.completions, ['deploying']);
  assert.deepEqual(labels.hits.map((h) => h.id), ['Message-2']);
});
