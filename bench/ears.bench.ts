// The engine's benchmark: hydrating an app-sized graph, then queries, relation walks and write batches on it.
// Run with `npm run bench -w @apack/ears`; the baseline and tolerance are in docs/goals/goal-package-boundaries.md (Phase 6).
import { bench, describe } from 'vitest';
import { EARS, type PersistenceSink } from '../src/index.ts';
import { freshEngine } from '../tests/engine/engine-under-test.ts';

const TASKS = 40_000;
const PROJECTS = 5_000;
const PEOPLE = 5_000;
const BLOCKS = 15_000;

type Id = EARS.EntityId;
const types = new Set(['Task', 'Project', 'Person', 'Note', 'Relation']);
// A sink that does nothing, as the LMDB store's does between flushes
const sink: PersistenceSink = {
  onCreateEntity() {}, onDestroyEntity() {}, onDropAttr() {}, onPutAttrArray() {},
  onAddRelation() {}, onUpdateRelation() {}, onRemoveRelation() {},
};
const engine = freshEngine({ isEntityType: (name) => types.has(name), persistence: sink });
const { query, admin } = engine;

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
  });
  let rel = 0;
  const relate = (src: Id, kind: string, tgt: Id) => {
    const relId = `Relation-${rel++}` as Id;
    admin.bulkLoadAttr(relId, EARS.AttrKind.RelationDetails, { sourceEntity: src, targetEntity: tgt, relationType: kind, info: undefined });
    admin.addToIndex(kind, src, tgt, relId);
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
  bench('qx by type with where and pickAll', () => {
    query.qx('Task').where('status', 'blocked').pickAll();
  }, { iterations: 20, time: 0 });

  bench('1,000 qx(id) lookups', () => {
    for (let i = 0; i < 1000; i++) query.qx(task(i)).count();
  }, { iterations: 20, time: 0 });

  bench('qx by type with 3 chained steps', () => {
    query.qx('Task').ofType('Task').orderBy('rank').limit(10).ids();
  }, { iterations: 20, time: 0 });

  bench('relation traversal (a project: its tasks, then their assignees)', () => {
    query.qx(project(1)).linksTo('contains', 'Task').linksTo('assigned').count();
  }, { iterations: 30, time: 0 });
});

describe('writes', () => {
  bench('tx batch of 1,000 creates', () => {
    for (let i = 0; i < 1000; i++) query.tx('Note').put('title', `Note ${i}`).put('status', 'open');
  }, { iterations: 50, time: 0 });
});
