// The engine's contract: what it tells its persistence sink, in order, for a sequence of writes; and hydration
// through bulk loading, which tells the sink nothing
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { EARS } from '../../src/index.ts';
import { engine, recordingSink, type Id } from './helpers.ts';

beforeEach(() => {
  vi.useFakeTimers({ now: 1000, toFake: ['Date'] });
});
afterEach(() => vi.useRealTimers());

describe('persistence sink', () => {
  it('receives each write, in order, with its payload: a created entity with its type, and the relations a destroy removes before the entity', () => {
    const { sink, calls } = recordingSink();
    const e = engine(sink);
    const a = e.create('Task').set('title', 'A').append('tag', 'x').append('tag', 'y').id();
    const b = e.create('Project').setMany({ title: 'P' }).id();
    e.write(a).merge('meta', { k: 1 }).removeAt('tag', 0).removeValue('tag', 'y').grant('lead');
    e.write(b).link('contains', a, { info: { order: 1 } });
    const rel = e.findRelations()[0].id;
    e.updateRelation(rel, { info: { order: 2 } });
    e.write(b).link('contains', a, { replace: true });
    e.write(a).set('title', 'B').delete('gone');
    e.write(a).destroy();

    expect(calls).toEqual([
      ['onCreateEntity', '#1', 'Task'],
      ['onPutAttr', 'createdAt', '#1', 1000],
      ['onPutAttr', 'updatedAt', '#1', 1000],
      ['onPutAttr', 'title', '#1', 'A'],
      ['onPutAttr', 'tag', '#1', ['x']],
      ['onPutAttr', 'tag', '#1', ['x', 'y']],
      ['onCreateEntity', '#2', 'Project'],
      ['onPutAttr', 'createdAt', '#2', 1000],
      ['onPutAttr', 'updatedAt', '#2', 1000],
      ['onPutAttr', 'title', '#2', 'P'],
      // A write's first attribute change stamps updatedAt, once for the builder
      ['onPutAttr', 'meta', '#1', { k: 1 }],
      ['onPutAttr', 'updatedAt', '#1', 1000],
      ['onPutAttr', 'tag', '#1', ['y']],
      ['onPutAttr', 'tag', '#1', []],
      ['onPutAttr', 'role', '#1', ['lead']],
      ['onAddRelation', '#3', 'contains', '#2', '#1', { order: 1 }],
      ['onUpdateRelation', '#3', { info: { order: 2 } }],
      ['onRemoveRelation', '#3'],
      ['onAddRelation', '#4', 'contains', '#2', '#1', undefined],
      ['onPutAttr', 'title', '#1', 'B'],
      ['onPutAttr', 'updatedAt', '#1', 1000],
      ['onRemoveRelation', '#4'],
      ['onDestroyEntity', '#1'],
    ]);
  });

  it('receives a moved relation\'s new ends, and the direct attribute and relation writes', () => {
    const { sink, calls } = recordingSink();
    const e = engine(sink);
    const [a, b, c] = ['Task', 'Task', 'Task'].map((type) => e.create(type).id());
    const rel = e.admin.addRelation(a, 'refs', b);
    e.admin.updateRelation(rel, { target: c, info: 'why' });
    e.admin.updateRelation('Relation-none' as Id, { source: a });
    e.admin.putAttr(a, 'x', 1);
    e.admin.putAttr(a, 'x', [1, 2]);
    e.admin.deleteAttr(a, 'x');
    e.admin.deleteAttr(a, 'x');
    // Each create: the entity with its type, then its createdAt and updatedAt; `entityType` is the id's, never stored
    expect(calls.slice(0, 3)).toEqual([
      ['onCreateEntity', '#1', 'Task'],
      ['onPutAttr', 'createdAt', '#1', 1000],
      ['onPutAttr', 'updatedAt', '#1', 1000],
    ]);
    // A relation's details reach the sink as its row (onAddRelation, onUpdateRelation), never as an attribute; the
    // admin's direct writes stamp nothing
    expect(calls.slice(9)).toEqual([
      ['onAddRelation', '#4', 'refs', '#1', '#2', undefined],
      ['onUpdateRelation', '#4', { target: '#3', info: 'why' }],
      ['onPutAttr', 'x', '#1', 1],
      ['onPutAttr', 'x', '#1', [1, 2]],
      // Removing what isn't there tells the sink nothing
      ['onDropAttr', 'x', '#1'],
    ]);
    expect(e.query.ids(a).out('refs').ids()).toEqual([c]);
  });
});

it('hears a destroyed relation as that relation removed, not as an entity destroyed', () => {
  const { sink, calls } = recordingSink();
  const e = engine(sink);
  const [a, b] = ['Task', 'Task'].map((type) => e.create(type).id());
  e.write(a).link('refs', b);
  const before = calls.length;
  e.write(e.findRelations()[0].id).destroy();
  expect(calls.slice(before)).toEqual([['onRemoveRelation', '#3']]);
});

describe('hydration', () => {
  it('bulk loads attributes and relations without telling the sink, then answers queries', () => {
    const { sink, calls } = recordingSink();
    const e = engine(sink);
    const [a, b, p, rel] = ['Task-a1', 'Task-b1', 'Project-p1', 'Relation-r1'] as Id[];
    e.admin.bulkLoadAttr(a, 'title', 'A');
    e.admin.bulkLoadAttr(a, 'tag', ['first', 'second']);
    e.admin.bulkLoadAttr(a, 'meta', { x: 1, y: 2 });
    e.admin.bulkLoadAttr(b, 'title', 'B');
    e.admin.bulkLoadAttr(p, 'title', 'P');
    e.admin.bulkLoadAttr(p, EARS.AttrKind.Role, ['current']);
    e.admin.bulkLoadAttr(rel, EARS.AttrKind.RelationDetails, { source: p, target: a, kind: 'contains', info: { order: 1 } });
    e.admin.addToIndex(rel, 'contains', p, a);
    e.admin.addToIndex(rel, 'contains', p, a);

    expect(calls).toEqual([]);
    expect(e.admin.getAllEntities()).toEqual([a, b, p, rel]);
    expect(e.query.ids(a).pickOne()).toEqual({ id: a, entityType: 'Task', title: 'A', tag: ['first', 'second'], meta: { x: 1, y: 2 } });
    expect(e.query('Task').orderBy('title', 'desc').pick(['title'])).toEqual([{ id: b, title: 'B' }, { id: a, title: 'A' }]);
    expect(e.query.all().withRole('current').ids()).toEqual([p]);
    expect(e.query.ids(p).out('contains').ids()).toEqual([a]);
    expect(e.findRelations({ target: a })).toEqual([{ id: rel, source: p, target: a, kind: 'contains', info: { order: 1 } }]);

    // Writes after hydration reach the sink
    e.write(b).link('contains', a);
    expect(calls.map(([method]) => method)).toEqual(['onAddRelation']);

    e.admin.clear();
    expect(e.admin.getAllEntities()).toEqual([]);
    expect(Object.keys(e.getSchemaStats().attributes)).toEqual([]);
    expect(e.findRelations()).toEqual([]);
    expect(calls).toHaveLength(1);
  });
});
