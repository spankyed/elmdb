// The engine's heap with the benchmark's graph loaded (50k entities, 200k attributes, 100k relations), in the
// built engine: `npm run bench:heap -w @apackdev/ears`. The baseline is in docs/plans/ears-storage-layout.md (Step 4).
import type { EARS as EarsTypes } from '../src/index.ts';

type Id = EarsTypes.EntityId;
// A specifier held in a variable, so the typecheck (which runs before any build) reads the types from source
const DIST = '../dist/index.js';
const { createEarsEngine } = await import(DIST) as typeof import('../src/index.ts');
const gc = (globalThis as { gc?: () => void }).gc;
if (!gc) throw new Error('Run with node --expose-gc');

const types = new Set(['Task', 'Project', 'Person']);
const STATUSES = ['open', 'done', 'blocked'];
gc();
const before = process.memoryUsage().heapUsed;
const engine = createEarsEngine({ isEntityType: (name) => types.has(name), isRelationKind: () => true, isRole: () => true });
const { admin } = engine;
const ids: Id[] = [];
for (let i = 0; i < 40_000; i++) ids.push(`Task-${i}` as Id);
for (let i = 0; i < 5_000; i++) ids.push(`Project-${i}` as Id);
for (let i = 0; i < 5_000; i++) ids.push(`Person-${i}` as Id);
// Loaded as hydration loads, in one unit
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
console.log(`${(used / 1e6).toFixed(1)} MB of heap for ${engine.query('Task').count() + 10_000} entities, 200k attributes, 100k relations`);
