// What `rank` saves a caller (`docs/archive/roadmap/graph-algorithms.md`, Verification, Cost): `node
// crates/elmdb-node/bench/rank.mjs [ROWS...]` after `npm run build`. For each size it builds documents of five `refs`
// edges each, as `tests/support/mod.rs`'s `Fixture::graph` does, then times personalized PageRank from the first
// document, its ten highest answered, two ways: every edge read through the binding and ranked in JavaScript, and
// the `rank` step in the engine. Both answer the same ten, which is checked first. Medians of several runs.
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir, cpus } from 'node:os';
import { join } from 'node:path';
import { Packr } from 'msgpackr';

const require = createRequire(import.meta.url);
const { Database } = require('../index.js');
const packr = new Packr({ useRecords: false, mapsAsObjects: true });

const sizes = process.argv.slice(2).map(Number);
const ROWS = sizes.length ? sizes : [1000, 10_000, 100_000];
const DEGREE = 5;
const id = (i) => `Doc-${String(i).padStart(8, '0')}`;
const schema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  types: { Doc: { fields: { body: { type: 'string' } } } },
  relations: { refs: { cardinality: 'many-to-many' } },
};
const read = (db, plan) => packr.unpack(db.readSync(packr.pack({ reads: [{ read: 'query', plan }] })))[0];
const median = (xs) => xs.sort((a, b) => a - b)[Math.floor(xs.length / 2)];
const time = (runs, f) => {
  const ns = [];
  let answer;
  for (let n = 0; n < runs; n++) {
    const t = process.hrtime.bigint();
    answer = f();
    ns.push(Number(process.hrtime.bigint() - t));
  }
  return [median(ns) / 1e6, answer];
};

/** Personalized PageRank as the engine computes it, written as a caller would: edges both ways, from one seed */
function inJavaScript(db) {
  const ids = read(db, { start: { from: 'types', types: ['Doc'] }, terminal: { answer: 'ids' } }).ids;
  const relations = read(db, {
    start: { from: 'types', types: ['Doc'] },
    terminal: { answer: 'edges', kinds: ['refs'], direction: 'out' },
  }).relations;
  const place = new Map(ids.map((id, at) => [id, at]));
  const out = ids.map(() => []);
  for (const r of relations) {
    const s = place.get(r.source), t = place.get(r.target);
    out[s].push(t);
    out[t].push(s);
  }
  const n = ids.length, d = 0.85;
  const teleport = new Float64Array(n);
  teleport[0] = 1;
  let scores = Float64Array.from(teleport), next = new Float64Array(n);
  for (let round = 0; round < 100; round++) {
    next.fill(0);
    let dangling = 0;
    for (let v = 0; v < n; v++) {
      if (out[v].length === 0) dangling += scores[v];
      else for (const w of out[v]) next[w] += scores[v] / out[v].length;
    }
    let change = 0;
    for (let v = 0; v < n; v++) {
      next[v] = d * next[v] + (d * dangling + 1 - d) * teleport[v];
      change += Math.abs(next[v] - scores[v]);
    }
    [scores, next] = [next, scores];
    if (change < 1e-6) break;
  }
  return [...scores.keys()].sort((a, b) => scores[b] - scores[a] || a - b).slice(0, 10).map((at) => ids[at]);
}

const inEngine = (db) => read(db, {
  start: { from: 'types', types: ['Doc'] },
  steps: [{ step: 'rank', kinds: ['refs'], direction: 'both', seeds: [id(0)] }, { step: 'limit', n: 10 }],
  terminal: { answer: 'hits' },
}).hits.map((hit) => hit.id);

console.log(`${new Date().toISOString().slice(0, 10)}, Node ${process.version}, ${cpus()[0].model}, ${DEGREE} edges a document`);
for (const rows of ROWS) {
  const dir = mkdtempSync(join(tmpdir(), 'elmdb-rank-bench-'));
  const db = Database.open(join(dir, 'db'), { schema, syncs: false });
  try {
    const loads = Array.from({ length: rows }, (_, i) => ({ load: 'entity', id: id(i), fields: { body: 'd' }, trashed: false }));
    for (let i = 0; i < rows; i++) {
      const targets = [...new Set(Array.from({ length: DEGREE }, (_, j) => (i * 31 + (j + 1) * 977) % rows))].filter((t) => t !== i);
      for (const t of targets.sort((a, b) => a - b)) {
        loads.push({ load: 'relation', id: `relation-${i}-${t}`, kind: 'refs', source: id(i), target: id(t) });
      }
    }
    db.bulkLoad(packr.pack(loads));
    assert.deepEqual(inJavaScript(db), inEngine(db), 'both answer the same ten');
    const runs = rows >= 100_000 ? 5 : 20;
    const [js] = time(runs, () => inJavaScript(db));
    const [engine] = time(runs, () => inEngine(db));
    console.log(`${rows} documents: through the binding and JavaScript ${js.toFixed(2)} ms, rank ${engine.toFixed(2)} ms, ${(js / engine).toFixed(1)}x, medians of ${runs}`);
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
}
