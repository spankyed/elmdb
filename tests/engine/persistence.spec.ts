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
    const a = e.query.tx('Task').put('title', 'A').add('tag', 'x').add('tag', 'y').id();
    const b = e.query.tx('Project').batchPut({ title: 'P' }).id();
    e.query.tx(a).merge('meta', { k: 1 }).drop('tag', 0).dropIf('tag', 'y').grant('lead');
    e.query.tx(b).link('contains', a, { order: 1 });
    const rel = e.query.findRelations()[0].id;
    e.query.tx(b).relPatch(rel, { info: { order: 2 } });
    e.query.tx(b).linkOne('contains', a);
    e.query.updateEntity(a, { title: 'B', gone: null });
    e.query.tx(a).destroy();
    e.query.tx(b).destroy(true);

    expect(calls).toEqual([
      ['onCreateEntity', '#1', 'Task'],
      ['onPutAttrArray', 'entityType', '#1', ['Task']],
      ['onPutAttrArray', 'createdAt', '#1', [1000]],
      ['onPutAttrArray', 'title', '#1', ['A']],
      ['onPutAttrArray', 'tag', '#1', ['x']],
      ['onPutAttrArray', 'tag', '#1', ['x', 'y']],
      ['onCreateEntity', '#2', 'Project'],
      ['onPutAttrArray', 'entityType', '#2', ['Project']],
      ['onPutAttrArray', 'createdAt', '#2', [1000]],
      ['onPutAttrArray', 'title', '#2', ['P']],
      ['onPutAttrArray', 'meta', '#1', [{ k: 1 }]],
      ['onPutAttrArray', 'tag', '#1', ['y']],
      ['onDropAttr', 'tag', '#1', 0, []],
      ['onPutAttrArray', 'role', '#1', ['lead']],
      ['onPutAttrArray', 'relationDetails', '#3', [{ sourceEntity: '#2', targetEntity: '#1', relationType: 'contains', info: { order: 1 } }]],
      ['onAddRelation', '#3', 'contains', '#2', '#1', { order: 1 }],
      ['onPutAttrArray', 'relationDetails', '#3', [{ sourceEntity: '#2', targetEntity: '#1', relationType: 'contains', info: { order: 2 } }]],
      ['onUpdateRelation', '#3', { info: { order: 2 } }],
      ['onDropAttr', 'relationDetails', '#3', 0, []],
      ['onRemoveRelation', '#3'],
      ['onPutAttrArray', 'relationDetails', '#4', [{ sourceEntity: '#2', targetEntity: '#1', relationType: 'contains', info: undefined }]],
      ['onAddRelation', '#4', 'contains', '#2', '#1', undefined],
      ['onPutAttrArray', 'updatedAt', '#1', [1000]],
      ['onPutAttrArray', 'title', '#1', ['B']],
      ['onDropAttr', 'relationDetails', '#4', 0, []],
      ['onRemoveRelation', '#4', '#1'],
      ['onDestroyEntity', '#1'],
    ]);
  });

  it('receives a moved relation\'s new ends, and the direct attribute and relation writes', () => {
    const { sink, calls } = recordingSink();
    const e = engine(sink);
    const [a, b, c] = ['Task', 'Task', 'Task'].map((type) => e.query.tx(type).id());
    const rel = e.admin.addRelation(a, 'refs', b);
    e.admin.updateRelation(rel, undefined, c, 'why');
    e.admin.updateRelation('Relation-none' as Id, a);
    e.admin.putAttr(a, 'x', 1);
    e.admin.addAttr(a, 'x', 2);
    e.admin.mergeAttr(a, 'x', 3, 1);
    e.admin.updateAttr(a, 'x', 4);
    e.admin.dropIf(a, 'x', 4);
    e.admin.dropAttr(a, 'x');
    // Each create: the entity, then the two attributes every new entity gets
    expect(calls.slice(0, 3)).toEqual([
      ['onCreateEntity', '#1', 'Task'],
      ['onPutAttrArray', 'entityType', '#1', ['Task']],
      ['onPutAttrArray', 'createdAt', '#1', [1000]],
    ]);
    expect(calls.slice(9)).toEqual([
      ['onPutAttrArray', 'relationDetails', '#4', [{ sourceEntity: '#1', targetEntity: '#2', relationType: 'refs', info: undefined }]],
      ['onAddRelation', '#4', 'refs', '#1', '#2', undefined],
      ['onPutAttrArray', 'relationDetails', '#4', [{ sourceEntity: '#1', targetEntity: '#3', relationType: 'refs', info: 'why' }]],
      ['onUpdateRelation', '#4', { tgt: '#3', info: 'why' }],
      ['onPutAttrArray', 'x', '#1', [1]],
      ['onPutAttrArray', 'x', '#1', [1, 2]],
      ['onPutAttrArray', 'x', '#1', [1, 3]],
      ['onPutAttrArray', 'x', '#1', [4]],
      ['onDropAttr', 'x', '#1', 0, []],
    ]);
    expect(e.query.qx(a).linksTo('refs').ids()).toEqual([c]);
  });
});

describe('hydration', () => {
  it('bulk loads attributes and relations without telling the sink, then answers queries', () => {
    const { sink, calls } = recordingSink();
    const e = engine(sink);
    const [a, b, p, rel] = ['Task-a1', 'Task-b1', 'Project-p1', 'Relation-r1'] as Id[];
    e.admin.bulkLoadAttr(a, 'title', 'A');
    e.admin.bulkLoadAttr(a, 'tag', 'second', 1);
    e.admin.bulkLoadAttr(a, 'tag', 'first', 0);
    e.admin.bulkLoadAttr(a, 'meta', { x: 1 });
    e.admin.bulkLoadAttr(a, 'meta', { y: 2 });
    e.admin.bulkLoadAttr(b, 'title', 'B');
    e.admin.bulkLoadAttr(p, 'title', 'P');
    e.admin.bulkLoadAttr(p, EARS.AttrKind.Role, 'current');
    e.admin.bulkLoadAttr(rel, EARS.AttrKind.RelationDetails, { sourceEntity: p, targetEntity: a, relationType: 'contains', info: { order: 1 } });
    e.admin.addToIndex('contains', p, a, rel);
    e.admin.addToIndex('contains', p, a, rel);

    expect(calls).toEqual([]);
    expect(e.query.getAllEntities()).toEqual([a, b, p, rel]);
    expect(e.query.findById(a)).toEqual({ id: a, title: 'A', tag: ['first', 'second'], meta: { x: 1, y: 2 } });
    expect(e.query.qx('Task').orderBy('title', 'desc').pick(['title'])).toEqual([{ id: b, title: 'B' }, { id: a, title: 'A' }]);
    expect(e.query.qx().withRole('current').ids()).toEqual([p]);
    expect(e.query.qx(p).linksTo('contains').ids()).toEqual([a]);
    expect(e.query.findRelations({ targetEntity: a })).toEqual([{ id: rel, sourceEntity: p, targetEntity: a, relationType: 'contains', info: { order: 1 } }]);
    expect(e.admin.edgeStore.find({ sourceEntity: p })).toEqual([{ sourceEntity: p, targetEntity: a, relationType: 'contains', info: { order: 1 } }]);
    expect(e.admin.edgeStore.relIds({})).toEqual([rel]);

    // Writes after hydration reach the sink
    e.query.tx(b).link('contains', a);
    expect(calls.map(([method]) => method)).toEqual(['onPutAttrArray', 'onAddRelation']);
    e.admin.removeFromIndex('contains', p, a, rel);
    expect(e.query.qx(p).linksTo('contains').ids()).toEqual([]);
    e.admin.updateIndex('contains', rel, p, a, b);
    expect(e.query.qx(b).linksTo('contains').ids()).toEqual([a]);

    e.admin.clear();
    expect(e.query.getAllEntities()).toEqual([]);
    expect(e.query.getAllAttributeKinds()).toEqual([]);
    expect(e.admin.relationIndex).toEqual({});
    expect(calls).toHaveLength(2);
  });

  it('edgeStore links, patches and unlinks', () => {
    const e = engine();
    const [a, b, c] = ['Task', 'Task', 'Task'].map((type) => e.query.tx(type).id());
    e.admin.edgeStore.linkOne(a, 'refs', b, 1);
    const rel = e.admin.edgeStore.linkOne(a, 'refs', b, 2);
    expect(e.admin.edgeStore.relIds({ sourceEntity: a, targetEntity: b })).toEqual([rel]);
    expect(e.admin.edgeStore.patchOne({ sourceEntity: a, relationType: 'refs' }, { newTarget: c, newInfo: 3 })).toBe(true);
    expect(e.admin.edgeStore.patchOne({ sourceEntity: b }, { newTarget: c })).toBe(false);
    expect(e.admin.edgeStore.find({ targetEntity: c })).toEqual([{ sourceEntity: a, targetEntity: c, relationType: 'refs', info: 3 }]);
    e.admin.edgeStore.unlink({ relationType: 'refs' });
    expect(e.query.findRelations()).toEqual([]);
  });
});
