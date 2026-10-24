// The engine's contract: the admin face's loads, which write stored rows as they were, unchecked, keeping their ids
import { describe, expect, it } from 'vitest';
import { EARS } from '../../src/index.ts';
import { engine, type Id, type RelId } from './helpers.ts';

describe('bulk loads', () => {
  it('loads attributes and relations as they were stored, unchecked, then answers queries', () => {
    const e = engine();
    const [a, b, p] = ['Task-a1', 'Task-b1', 'Project-p1'] as Id[];
    const rel = 'Relation-r1' as RelId;
    e.admin.bulkLoadAttr(a, 'title', 'A');
    e.admin.bulkLoadAttr(a, 'tag', ['first', 'second']);
    e.admin.bulkLoadAttr(a, 'meta', { x: 1, y: 2 });
    e.admin.bulkLoadAttr(b, 'title', 'B');
    e.admin.bulkLoadAttr(p, 'title', 'P');
    e.admin.bulkLoadAttr(p, EARS.AttrKind.Role, ['current']);
    e.admin.bulkLoadRelation(rel, 'contains', p, a, { order: 1 });
    e.admin.bulkLoadRelation(rel, 'contains', p, a, { order: 1 });

    expect(e.admin.getAllEntities()).toEqual([a, b, p]);
    expect(e.query.ids(a).pickOne()).toEqual({ id: a, entityType: 'Task', title: 'A', tag: ['first', 'second'], meta: { x: 1, y: 2 } });
    expect(e.query('Task').orderBy('title', 'desc').pick(['title'])).toEqual([{ id: b, title: 'B' }, { id: a, title: 'A' }]);
    expect(e.query.all().withRole('current').ids()).toEqual([p]);
    expect(e.query.ids(p).out('contains').ids()).toEqual([a]);
    expect(e.findRelations({ target: a })).toEqual([{ id: rel, source: p, target: a, kind: 'contains', info: { order: 1 } }]);

    e.admin.clear();
    expect(e.admin.getAllEntities()).toEqual([]);
    expect(Object.keys(e.getSchemaStats().attributes)).toEqual([]);
    expect(e.findRelations()).toEqual([]);
  });
});
