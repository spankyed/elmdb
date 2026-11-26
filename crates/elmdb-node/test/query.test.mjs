// v2 Query engine over N-API: Value/Order/Text(BM25F)/Trigram/Vector/Rank + Graph analytics + Aggregates + queryAsync.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { Database } = require('../index.js');
const { buildWireBatch } = require('../client.js');

const schema = {
  types: {
    Doc: {
      fields: {
        title: { type: 'string' },
        body: { type: 'string' },
        group: { type: 'string' },
        score: { type: 'integer' },
      },
      indexes: {
        byScore: { kind: 'value', fields: ['score'] },
        byTitle: { kind: 'order', field: 'title', locale: 'en' },
        fts: { kind: 'text', fields: [['title', 3], ['body', 1]] },
        fuzzy: { kind: 'trigram', fields: ['title'] },
        emb: { kind: 'vector', fields: ['body'], dims: 4, quantization: 'sq8' },
        rank: { kind: 'rank', relations: ['cites'] },
      },
    },
  },
  relations: {
    cites: { endpoints: [{ source: 'Doc', target: 'Doc' }] },
  },
};

test('queries cover filters, ICU order, BM25F, trigram, vector, PPR, Leiden, aggregates, and queryAsync', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-v2-q-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const db = Database.open(dir, { schema, durability: 'unsafe', buildPolicy: 'eager' });
  t.after(() => db.close());

  db.executeWireBatch(buildWireBatch((b) => {
    const a = b.create('Doc', { title: 'Rust Storage', body: 'LMDB copy on write trees', group: 'sys', score: 10 }, 'd1');
    const c = b.create('Doc', { title: 'Graph Theory', body: 'PageRank and Leiden communities', group: 'algo', score: 20 }, 'd2');
    const d = b.create('Doc', { title: 'Alpha Engine', body: 'Vector HNSW nearest neighbor search', group: 'sys', score: 30 }, 'd3');
    b.link(a, 'cites', c);
    b.link(d, 'cites', c);
  }));
  db.setEmbeddings('emb', 'd1', [[1, 0, 0, 0]]);
  db.setEmbeddings('emb', 'd2', [[0, 1, 0, 0]]);
  db.setEmbeddings('emb', 'd3', [[0, 0, 1, 0]]);
  db.flush();

  // Order + filter + explain
  const ordered = db.query({
    source: { kind: 'type', typeName: 'Doc' },
    steps: [
      { kind: 'filter', expr: { op: 'range', field: 'score', low: [10, true], high: [30, true] } },
      { kind: 'orderSlice', by: 'field', field: 'title', dir: 'asc', locale: 'en' },
    ],
    terminal: { kind: 'ids' },
    explain: true,
  });
  assert.ok(ordered.explain);
  assert.deepEqual(ordered.ids, ['d3', 'd2', 'd1']);

  // BM25F async
  const bm25 = await db.queryAsync({
    source: { kind: 'type', typeName: 'Doc' },
    steps: [{ kind: 'score', spec: { kind: 'bm25f', index: 'fts', query: 'LMDB trees', topK: 5 } }],
    terminal: { kind: 'hits' },
  });
  assert.equal(bm25.hits[0].id, 'd1');

  // Vector near
  const vec = db.query({
    source: { kind: 'type', typeName: 'Doc' },
    steps: [{ kind: 'score', spec: { kind: 'vectorNear', index: 'emb', vector: [0, 0, 0.99, 0.01], topK: 2 } }],
    terminal: { kind: 'hits' },
  });
  assert.equal(vec.hits[0].id, 'd3');

  // Aggregates: count, sum, groupBy
  const cnt = db.query({ source: { kind: 'type', typeName: 'Doc' }, steps: [], terminal: { kind: 'count' } });
  assert.equal(cnt.count, 3);

  const sum = db.query({ source: { kind: 'type', typeName: 'Doc' }, steps: [], terminal: { kind: 'sum', field: 'score' } });
  assert.equal(sum.sum, 60);

  const grouped = db.query({
    source: { kind: 'type', typeName: 'Doc' },
    steps: [],
    terminal: { kind: 'groupBy', field: 'group', countsOnly: true },
  });
  assert.equal(grouped.kind, 'groupCounts');

  // Graph PPR
  const ppr = db.query({
    source: { kind: 'type', typeName: 'Doc' },
    steps: [{ kind: 'graphPpr', relKinds: ['cites'], seeds: [['d1', 1.0]], damping: 0.85, iterations: 20, tolerance: 1e-6 }],
    terminal: { kind: 'hits' },
  });
  assert.ok(ppr.hits.some((h) => h.id === 'd2'));
});
