// The engine's read cases on data shaped like the production database (production-shape.ts): its types and their
// sizes, its large values, its relations. Run with `npm run bench -w @apackdev/ears`, which builds the package first
// and runs this beside ears.bench.ts. On an LMDB store in a temp directory; `EARS_BENCH_KV=memory` runs it on
// `createMemoryKV`. The readings are in docs/plans/ears-storage-layout.md (Step 5)
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterAll, bench, describe } from 'vitest';
import { loadProductionShape, productionId, TYPES } from './production-shape.ts';

// Specifiers held in variables, so the typecheck (which runs before any build) reads the types from source
const DIST = '../dist/index.js';
const DIST_LMDB = '../dist/lmdb/index.js';
const { createEarsEngine } = await import(/* @vite-ignore */ DIST) as typeof import('../src/index.ts');
const { openLmdbStore } = await import(/* @vite-ignore */ DIST_LMDB) as typeof import('../src/lmdb/index.ts');

const onMemory = process.env.EARS_BENCH_KV === 'memory';
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-production-bench-'));
const store = onMemory ? undefined : openLmdbStore({ path: path.join(dir, 'ears-db') });
afterAll(() => {
  store?.close();
  fs.rmSync(dir, { recursive: true, force: true });
});
const engine = createEarsEngine({ ...(store && { kv: store.kv }), isEntityType: (name) => name in TYPES, isRelationKind: () => true, isRole: () => true });
loadProductionShape(engine.admin);

const MESSAGES = TYPES.Message!.entities;
const THREADS = TYPES.Thread!.entities;
/** What the thread view picks of each message (`threadQueries.messages`) */
const MESSAGE_FIELDS = ['text', 'sender', 'timestamp', 'compacted', 'isCommand'] as const;

describe('production-shaped reads', () => {
  // Opening a thread: its messages through its `contains` edges, about 58 each, as the thread view reads them
  bench('100 threads opened (their messages, picked)', () => {
    for (let i = 0; i < 100; i++) engine.query.ids(productionId('Thread', i)).out('contains', 'Message').pick(MESSAGE_FIELDS);
  }, { iterations: 20, time: 0 });

  // Rows by id across the message range, which holds the large `blocks` values
  bench('1,000 getRow on messages', () => {
    for (let i = 0; i < 1000; i++) engine.getRow(productionId('Message', (i * 19) % MESSAGES));
  }, { iterations: 20, time: 0 });

  bench('1,000 query(Message).first()', () => {
    for (let i = 0; i < 1000; i++) engine.query('Message').first();
  }, { iterations: 10, time: 0 });

  // The thread list: every thread, whole
  bench('pickAll over the threads', () => {
    engine.query('Thread').pickAll();
  }, { iterations: 20, time: 0 });

  // A where on a field no type here indexes: a filtered walk of the threads
  bench('100 where lookups on the threads', () => {
    for (let i = 0; i < 100; i++) engine.query('Thread').where('topic', `${i}:`.padEnd(38, 'x')).first();
  }, { iterations: 10, time: 0 });

  bench('1,000 findRelations from a thread', () => {
    for (let i = 0; i < 1000; i++) engine.findRelations({ source: productionId('Thread', i % THREADS), kind: 'contains' });
  }, { iterations: 10, time: 0 });

  // Every message whole: the read no product path makes, its large values included
  bench('pickAll over the messages', () => {
    engine.query('Message').pickAll();
  }, { iterations: 5, time: 0 });
});
