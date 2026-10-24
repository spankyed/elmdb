// The engine's heap with the benchmark's graph loaded (50k entities, 200k attributes, 100k relations), in the
// built engine: `npm run bench:heap -w @apackdev/ears`. It runs on an LMDB store in a temp dir, as the app does, and
// then reads what opening that store again costs, which is the app's boot; `EARS_BENCH_KV=memory` runs it on
// `createMemoryKV` instead, which holds the graph in the heap. The readings are in docs/plans/ears-storage-layout.md.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import type { EARS as EarsTypes } from '../src/index.ts';

type Id = EarsTypes.EntityId;
// Specifiers held in variables, so the typecheck (which runs before any build) reads the types from source
const DIST = '../dist/index.js';
const DIST_LMDB = '../dist/lmdb/index.js';
const { createEarsEngine } = await import(DIST) as typeof import('../src/index.ts');
const { openLmdbStore } = await import(DIST_LMDB) as typeof import('../src/lmdb/index.ts');
const gc = (globalThis as { gc?: () => void }).gc;
if (!gc) throw new Error('Run with node --expose-gc');

const onMemory = process.env.EARS_BENCH_KV === 'memory';
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-heap-'));
const dbPath = path.join(dir, 'ears-db');
const types = new Set(['Task', 'Project', 'Person']);
const STATUSES = ['open', 'done', 'blocked'];
const engineOn = (kv?: import('../src/index.ts').OrderedKV) =>
  createEarsEngine({ kv, isEntityType: (name) => types.has(name), isRelationKind: () => true, isRole: () => true });
const mb = (bytes: number) => `${(bytes / 1e6).toFixed(1)} MB`;

try {
  gc();
  const before = process.memoryUsage().heapUsed;
  const store = onMemory ? undefined : openLmdbStore({ path: dbPath });
  const engine = engineOn(store?.kv);
  const { admin } = engine;
  const ids: Id[] = [];
  for (let i = 0; i < 40_000; i++) ids.push(`Task-${i}` as Id);
  for (let i = 0; i < 5_000; i++) ids.push(`Project-${i}` as Id);
  for (let i = 0; i < 5_000; i++) ids.push(`Person-${i}` as Id);
  // Loaded in one unit, as a conversion loads
  admin.bulkLoad(() => {
    ids.forEach((id, i) => {
      admin.bulkLoadAttr(id, 'title', `Row ${i}`);
      admin.bulkLoadAttr(id, 'status', STATUSES[i % 3]);
      admin.bulkLoadAttr(id, 'rank', i % 100);
      admin.bulkLoadAttr(id, 'createdAt', 1_700_000_000_000 + i);
    });
    let rel = 0;
    const relate = (source: string, kind: string, target: string) => {
      const relId = `Relation-${rel++}` as EarsTypes.RelationId;
      admin.bulkLoadRelation(relId, kind, source as Id, target as Id);
    };
    for (let i = 0; i < 40_000; i++) relate(`Project-${i % 5_000}`, 'contains', `Task-${i}`);
    for (let i = 0; i < 40_000; i++) relate(`Task-${i}`, 'assigned', `Person-${i % 5_000}`);
    for (let i = 0; i < 5_000; i++) relate(`Project-${i}`, 'owner', `Person-${i}`);
    for (let i = 0; i < 15_000; i++) relate(`Task-${i}`, 'blocks', `Task-${(i * 7 + 1) % 40_000}`);
  });
  gc();
  const used = process.memoryUsage().heapUsed - before;
  console.log(`${mb(used)} of heap for ${engine.query('Task').count() + 10_000} entities, 200k attributes, 100k relations (${onMemory ? 'memory' : 'LMDB'})`);
  if (store) {
    store.close();
    // The app's boot over these files: open the store, create the engine, count what it holds
    gc();
    const heapBefore = process.memoryUsage().heapUsed;
    const opened = performance.now();
    const again = openLmdbStore({ path: dbPath, readOnly: true });
    const reopened = engineOn(again.kv);
    const counted = reopened.admin.countEntities();
    const openMs = performance.now() - opened;
    const statsStarted = performance.now();
    reopened.getSchemaStats();
    const statsMs = performance.now() - statsStarted;
    gc();
    console.log(`boot: opened and counted ${Object.values(counted).reduce((a, b) => a + b, 0)} entities in ${openMs.toFixed(1)} ms, `
      + `heap ${mb(process.memoryUsage().heapUsed - heapBefore)} after; getSchemaStats ${statsMs.toFixed(0)} ms; `
      + `${mb(fs.statSync(path.join(dbPath, 'data.mdb')).size)} on disk`);
    again.close();
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
