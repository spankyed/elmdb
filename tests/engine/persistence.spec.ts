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
  it('receives each write, in order, with its payload: a created entity with its type, a relation a destroy removes with the destroyed end', () => {
    const { sink, calls } = recordingSink();
    const e = engine(sink);
    const a = e.create('Task').set('title', 'A').add('tag', 'x').add('tag', 'y').id();
    const b = e.create('Project').setMany({ title: 'P' }).id();
    e.write(a).merge('meta', { k: 1 }).deleteAt('tag', 0).deleteValue('tag', 'y').grant('lead');
    e.write(b).link('contains', a, { info: { order: 1 } });
    const rel = e.findRelations()[0].id;
    e.updateRelation(rel, { info: { order: 2 } });
    e.write(b).link('contains', a, { replace: true });
    e.updateEntity(a, { title: 'B', gone: null });
    e.write(a).destroy();
    e.write(b).destroy({ keepStored: true });

    expect(calls).toEqual([
      ['onCreateEntity', '#1', 'Task'],
      ['onPutAttr', 'createdAt', '#1', 1000],
      ['onPutAttr', 'title', '#1', 'A'],
      ['onPutAttr', 'tag', '#1', ['x']],
      ['onPutAttr', 'tag', '#1', ['x', 'y']],
      ['onCreateEntity', '#2', 'Project'],
      ['onPutAttr', 'createdAt', '#2', 1000],
      ['onPutAttr', 'title', '#2', 'P'],
      ['onPutAttr', 'meta', '#1', { k: 1 }],
      ['onPutAttr', 'tag', '#1', ['y']],
      ['onPutAttr', 'tag', '#1', []],
      ['onPutAttr', 'role', '#1', ['lead']],
      ['onAddRelation', '#3', 'contains', '#2', '#1', { order: 1 }],
      ['onUpdateRelation', '#3', { info: { order: 2 } }],
      ['onRemoveRelation', '#3'],
      ['onAddRelation', '#4', 'contains', '#2', '#1', undefined],
      ['onPutAttr', 'updatedAt', '#1', 1000],
      ['onPutAttr', 'title', '#1', 'B'],
      ['onRemoveRelation', '#4', '#1'],
      ['onDestroyEntity', '#1'],
    ]);
  });

  it('receives a moved relation\'s new ends, and the direct attribute and relation writes', () => {
    const { sink, calls } = recordingSink();
    const e = engine(sink);
    const [a, b, c] = ['Task', 'Task', 'Task'].map((type) => e.create(type).id());
    const rel = e.admin.addRelation(a, 'refs', b);
    e.admin.updateRelation(rel, undefined, c, 'why');
    e.admin.updateRelation('Relation-none' as Id, a);
    e.admin.putAttr(a, 'x', 1);
    e.admin.putAttr(a, 'x', [1, 2]);
    e.admin.deleteAttr(a, 'x');
    e.admin.deleteAttr(a, 'x');
    // Each create: the entity with its type, then its createdAt; `entityType` is the id's, never stored
    expect(calls.slice(0, 2)).toEqual([
      ['onCreateEntity', '#1', 'Task'],
      ['onPutAttr', 'createdAt', '#1', 1000],
    ]);
    // A relation's details reach the sink as its row (onAddRelation, onUpdateRelation), never as an attribute
    expect(calls.slice(6)).toEqual([
      ['onAddRelation', '#4', 'refs', '#1', '#2', undefined],
      ['onUpdateRelation', '#4', { tgt: '#3', info: 'why' }],
      ['onPutAttr', 'x', '#1', 1],
      ['onPutAttr', 'x', '#1', [1, 2]],
      // Removing what isn't there tells the sink nothing
      ['onDropAttr', 'x', '#1'],
    ]);
    expect(e.query.ids(a).linksTo('refs').ids()).toEqual([c]);
  });
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
    e.admin.bulkLoadAttr(rel, EARS.AttrKind.RelationDetails, { sourceEntity: p, targetEntity: a, relationType: 'contains', info: { order: 1 } });
    e.admin.addToIndex('contains', p, a, rel);
    e.admin.addToIndex('contains', p, a, rel);

    expect(calls).toEqual([]);
    expect(e.admin.getAllEntities()).toEqual([a, b, p, rel]);
    expect(e.findById(a)).toEqual({ id: a, entityType: 'Task', title: 'A', tag: ['first', 'second'], meta: { x: 1, y: 2 } });
    expect(e.query('Task').orderBy('title', 'desc').pick(['title'])).toEqual([{ id: b, title: 'B' }, { id: a, title: 'A' }]);
    expect(e.query.all().withRole('current').ids()).toEqual([p]);
    expect(e.query.ids(p).linksTo('contains').ids()).toEqual([a]);
    expect(e.findRelations({ targetEntity: a })).toEqual([{ id: rel, sourceEntity: p, targetEntity: a, relationType: 'contains', info: { order: 1 } }]);
    expect(e.admin.edgeStore.find({ sourceEntity: p })).toEqual([{ sourceEntity: p, targetEntity: a, relationType: 'contains', info: { order: 1 } }]);
    expect(e.admin.edgeStore.relIds({})).toEqual([rel]);

    // Writes after hydration reach the sink
    e.write(b).link('contains', a);
    expect(calls.map(([method]) => method)).toEqual(['onAddRelation']);
    e.admin.removeFromIndex('contains', p, a, rel);
    expect(e.query.ids(p).linksTo('contains').ids()).toEqual([]);
    e.admin.updateIndex('contains', rel, p, a, b);
    expect(e.query.ids(b).linksTo('contains').ids()).toEqual([a]);

    e.admin.clear();
    expect(e.admin.getAllEntities()).toEqual([]);
    expect(e.getAllAttributeKinds()).toEqual([]);
    expect(e.admin.relationIndex).toEqual({});
    expect(calls).toHaveLength(1);
  });

  it('edgeStore links, patches and unlinks', () => {
    const e = engine();
    const [a, b, c] = ['Task', 'Task', 'Task'].map((type) => e.create(type).id());
    e.admin.edgeStore.linkOne(a, 'refs', b, 1);
    const rel = e.admin.edgeStore.linkOne(a, 'refs', b, 2);
    expect(e.admin.edgeStore.relIds({ sourceEntity: a, targetEntity: b })).toEqual([rel]);
    expect(e.admin.edgeStore.patchOne({ sourceEntity: a, relationType: 'refs' }, { newTarget: c, newInfo: 3 })).toBe(true);
    expect(e.admin.edgeStore.patchOne({ sourceEntity: b }, { newTarget: c })).toBe(false);
    expect(e.admin.edgeStore.find({ targetEntity: c })).toEqual([{ sourceEntity: a, targetEntity: c, relationType: 'refs', info: 3 }]);
    e.admin.edgeStore.unlink({ relationType: 'refs' });
    expect(e.findRelations()).toEqual([]);
  });
});
