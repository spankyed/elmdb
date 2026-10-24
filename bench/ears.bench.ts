// The engine's benchmark: hydrating an app-sized graph, then queries, relation walks and write batches on it.
// Run with `npm run bench -w @apackdev/ears`, which builds the package first: it times the engine in `dist`, the build
// that ships, since a dev loader compiles with its own options. The baseline is in docs/plans/ears-storage-layout.md
// (Step 4); the tolerance is +10% median per case.
import { bench, describe } from 'vitest';
import type { EARS as EarsTypes, PersistenceSink } from '../src/index.ts';

// A specifier held in a variable, so the typecheck (which runs before any build) reads the types from source
const DIST = '../dist/index.js';
const { createEarsEngine, EARS } = await import(/* @vite-ignore */ DIST) as typeof import('../src/index.ts');

const TASKS = 40_000;
const PROJECTS = 5_000;
const PEOPLE = 5_000;
const BLOCKS = 15_000;

type Id = EarsTypes.EntityId;
const types = new Set(['Task', 'Project', 'Person', 'Note']);
// A sink that does nothing, as the LMDB store's does between flushes
const sink: PersistenceSink = {
  onCreateEntity() {}, onDestroyEntity() {}, onDropAttr() {}, onPutAttr() {},
  onAddRelation() {}, onUpdateRelation() {}, onRemoveRelation() {}, onUnitStart() {}, onUnitEnd() {},
};
// Task rows are numbered T-<n>, as an app's shapes number theirs
const engine = createEarsEngine({ isEntityType: (name) => types.has(name), isRelationKind: () => true, isRole: () => true, persistence: sink, rowRules: (type) => (type === 'Task' ? { required: [], shortCode: 'T', indexes: ['status', 'title'] } : undefined) });
const { admin } = engine;

const task = (i: number) => `Task-${i}` as Id;
const project = (i: number) => `Project-${i}` as Id;
const person = (i: number) => `Person-${i}` as Id;
const STATUSES = ['open', 'done', 'blocked'];

/** 50k entities with 4 attributes each (200k), and 100k relations */
function hydrate(): void {
  admin.clear();
  const entities: Id[] = [];
  for (let i = 0; i < TASKS; i++) entities.push(task(i));
  for (let i = 0; i < PROJECTS; i++) entities.push(project(i));
  for (let i = 0; i < PEOPLE; i++) entities.push(person(i));
  entities.forEach((id, i) => {
    admin.bulkLoadAttr(id, 'title', `Row ${i}`);
    admin.bulkLoadAttr(id, 'status', STATUSES[i % 3]);
    admin.bulkLoadAttr(id, 'rank', i % 100);
    admin.bulkLoadAttr(id, 'createdAt', 1_700_000_000_000 + i);
    // One entity in a hundred holds a role, and one in a thousand a short code: what an app's rows hold
    if (i % 100 === 0) admin.bulkLoadAttr(id, EARS.AttrKind.Role, ['pinned']);
    if (i % 1000 === 0) admin.bulkLoadAttr(id, 'shortCode', `T-${i}`);
  });
  let rel = 0;
  const relate = (src: Id, kind: string, tgt: Id) => {
    const relId = `Relation-${rel++}` as EarsTypes.RelationId;
    admin.bulkLoadRelation(relId, kind, src, tgt);
  };
  for (let i = 0; i < TASKS; i++) relate(project(i % PROJECTS), 'contains', task(i));
  for (let i = 0; i < TASKS; i++) relate(task(i), 'assigned', person(i % PEOPLE));
  for (let i = 0; i < PROJECTS; i++) relate(project(i), 'owner', person(i));
  for (let i = 0; i < BLOCKS; i++) relate(task(i), 'blocks', task((i * 7 + 1) % TASKS));
}

hydrate();

describe('hydrate', () => {
  bench('bulk load 50k entities, 200k attributes, 100k relations', hydrate, { iterations: 10, time: 0 });
});

describe('queries', () => {
  bench('query by type with where and pickAll', () => {
    engine.query('Task').where('status', 'blocked').pickAll();
  }, { iterations: 20, time: 0 });

  // A lookup by a value one entity holds, as a note's links and a document's short code are resolved
  bench('1,000 where lookups by a unique value', () => {
    for (let i = 0; i < 1000; i++) engine.query('Task').where('title', `Row ${i * 37}`).first();
  }, { iterations: 10, time: 0 });

  // A lookup on a field the type declares no index for, matching nothing: a filtered walk of the whole type
  bench('10 where lookups on an unindexed field', () => {
    for (let i = 0; i < 10; i++) engine.query('Task').where('rank', -1).first();
  }, { iterations: 10, time: 0 });

  bench('100 withRole lookups', () => {
    for (let i = 0; i < 100; i++) engine.query('Task').withRole('pinned').count();
  }, { iterations: 10, time: 0 });

  bench('1,000 query.ids(id) lookups', () => {
    for (let i = 0; i < 1000; i++) engine.query.ids(task(i)).count();
  }, { iterations: 20, time: 0 });

  bench('query by type with 3 chained steps', () => {
    engine.query('Task').ofType('Task').orderBy('rank').limit(10).ids();
  }, { iterations: 20, time: 0 });

  bench('relation traversal (a project: its tasks, then their assignees)', () => {
    engine.query.ids(project(1)).out('contains', 'Task').out('assigned').count();
  }, { iterations: 30, time: 0 });

  bench('orderBy a string over 40k', () => {
    engine.query('Task').orderBy('title').first();
  }, { iterations: 10, time: 0 });

  bench('1,000 query(type).first() over 40k', () => {
    for (let i = 0; i < 1000; i++) engine.query('Task').first();
  }, { iterations: 10, time: 0 });

  bench('1,000 getRow', () => {
    for (let i = 0; i < 1000; i++) engine.getRow(task(i * 37));
  }, { iterations: 20, time: 0 });

  bench('pickAll over 40k', () => {
    engine.query('Task').pickAll();
  }, { iterations: 10, time: 0 });

  bench('100 linkedWith', () => {
    for (let i = 0; i < 100; i++) engine.query('Project').linkedWith(person(i)).count();
  }, { iterations: 10, time: 0 });
});

describe('writes', () => {
  // A hub, as a collection holds its documents or a flow root its runs: linking is per edge, destroying per entity
  bench('link 10k tasks to one project, then destroy it', () => {
    const hub = engine.create('Project').set('title', 'Hub').id();
    for (let i = 0; i < 10_000; i++) engine.write(hub).link('contains', task(i));
    engine.write(hub).destroy();
  }, { iterations: 5, time: 0 });

  bench('1,000 creates', () => {
    for (let i = 0; i < 1000; i++) engine.create('Note').set('title', `Note ${i}`).set('status', 'open');
  }, { iterations: 50, time: 0 });

  bench('1,000 short codes', () => {
    for (let i = 0; i < 1000; i++) engine.create('Task');
  }, { iterations: 20, time: 0 });

  // The same writes as one unit: what the journal costs
  bench('transact: 1,000 creates as one unit', () => {
    engine.transact(() => {
      for (let i = 0; i < 1000; i++) engine.create('Note').set('title', `Note ${i}`).set('status', 'open');
    });
  }, { iterations: 50, time: 0 });
});
