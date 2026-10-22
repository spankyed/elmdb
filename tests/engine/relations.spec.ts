// The engine's contract: relations (links, their index and graph walks) and roles
import { beforeEach, describe, expect, it } from 'vitest';
import { engine, type Id } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

let e: EngineUnderTest;
const task = (title: string) => e.query.create('Task').set('title', title).id();
const edges = () => e.query.findRelations().map((r) => [r.sourceEntity, r.relationType, r.targetEntity, r.info]);

beforeEach(() => {
  e = engine();
});

describe('links', () => {
  it('link stores a Relation row with its details and indexes both ends', () => {
    const [a, b] = [task('A'), task('B')];
    e.query.write(a).link('blocks', b, { info: { weight: 2 } });
    const [rel] = e.query.findRelations();
    expect(rel).toEqual({ id: expect.stringMatching(/^Relation-/), sourceEntity: a, targetEntity: b, relationType: 'blocks', info: { weight: 2 } });
    expect(e.query.getAttr(rel.id, 'relationDetails')).toEqual({ sourceEntity: a, targetEntity: b, relationType: 'blocks', info: { weight: 2 } });
    expect(e.query.getEntitiesOfType('Relation')).toEqual([rel.id]);
    expect(e.query.qx.ids(a).linksTo('blocks').ids()).toEqual([b]);
    expect(e.query.qx.ids(b).linksTo('blocks', undefined, false).ids()).toEqual([a]);
    expect(e.query.qx.ids(a).edgeIds()).toEqual([rel.id]);
    expect(e.query.qx.ids(b).edgeIds('blocks', false)).toEqual([rel.id]);
    expect(e.query.qx.ids(a).edgeIds('missing')).toEqual([]);
    expect(e.query.queryEntitiesByRelationTo('blocks', a, true)).toEqual([b]);
    expect(e.query.queryEntitiesByRelationTo('blocks', b)).toEqual([a]);
    expect(e.query.queryEntitiesInRelationTo(a)).toEqual([b]);
    expect(e.query.getRelationStats('blocks')).toEqual({ total: 1, uniqueSources: 1, uniqueTargets: 1 });
    expect(e.query.getRelationStats('none')).toEqual({ total: 0, uniqueSources: 0, uniqueTargets: 0 });
    expect(e.admin.relationIndex).toEqual({ blocks: { bySource: { [a]: [rel.id] }, byTarget: { [b]: [rel.id] } } });
  });

  it('linking the same edge again changes nothing, keeping its id and its place; another info is another edge', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.query.write(a).link('blocks', b, { info: { x: 1 } }).link('blocks', c);
    const first = e.query.findRelations({ sourceEntity: a, targetEntity: b })[0].id;
    e.query.write(a).link('blocks', b, { info: { x: 1 } });
    // No info given matches any edge between the ends
    e.query.write(a).link('blocks', b);
    expect(e.query.findRelations().map((r) => r.id)).toEqual([first, expect.any(String)]);
    expect(e.query.qx.ids(a).linksTo('blocks').ids()).toEqual([b, c]);
    e.query.write(a).link('blocks', b, { info: { x: 2 } });
    expect(e.query.findRelations({ sourceEntity: a, targetEntity: b })).toHaveLength(2);
  });

  it('refuses options it does not know, which is info passed where the options go', () => {
    const [a, b] = [task('A'), task('B')];
    const info: any = { weight: 2 };
    expect(() => e.query.write(a).link('blocks', b, info)).toThrow("unknown option weight; an edge's info goes in { info }");
    expect(e.query.findRelations()).toEqual([]);
  });

  it('refuses a self link', () => {
    const a = task('A');
    expect(() => e.query.write(a).link('blocks', a)).toThrow("an entity can't be linked to itself");
    expect(() => e.query.write(a).link('blocks', a, { replace: true })).toThrow("can't be linked to itself");
    expect(() => e.query.linkSymmetric(a, a, 'peer')).toThrow('Self-link refused');
  });

  it('unlink, unlinkAll and removeRelationById remove relations and empty index entries', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.query.write(a).link('blocks', b).link('blocks', c).link('refs', b).link('refs', c);
    const blocksAB = e.query.findRelations({ sourceEntity: a, relationType: 'blocks', targetEntity: b })[0].id;
    e.query.removeRelationById(blocksAB);
    expect(edges()).toEqual([[a, 'blocks', c, undefined], [a, 'refs', b, undefined], [a, 'refs', c, undefined]]);
    e.query.write(a).unlink('refs', b);
    expect(edges()).toEqual([[a, 'blocks', c, undefined], [a, 'refs', c, undefined]]);
    e.query.write(a).unlink('blocks', c).unlink('refs', c);
    expect(edges()).toEqual([]);
    expect(e.query.getAllRelationKinds()).toEqual([]);
    // A removed relation's id stays in the type index, with no attributes
    expect(e.query.getEntitiesOfType('Relation')).toHaveLength(4);
    expect(e.query.qx('Relation').pickAll().map((row) => Object.keys(row))).toEqual([['id'], ['id'], ['id'], ['id']]);

    e.query.write(a).link('blocks', b).link('blocks', c).link('refs', c);
    e.query.write(a).unlink('blocks');
    expect(edges()).toEqual([[a, 'refs', c, undefined]]);
    e.query.write(c).link('refs', a);
    e.query.write(a).unlinkAll();
    // Only the edges from it
    expect(edges()).toEqual([[c, 'refs', a, undefined]]);
    e.query.write(c).unlinkAll();

    e.query.write(b).link('refs', c);
    e.query.removeRelationById(e.query.findRelations()[0].id);
    expect(edges()).toEqual([]);
    expect(e.admin.relationIndex).toEqual({});
  });

  it('createRelation and removeRelation link and unlink by ends', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.query.createRelation(a, 'refs', b);
    e.query.createRelation(a, 'refs', c);
    e.query.removeRelation(a, 'refs', b);
    expect(edges()).toEqual([[a, 'refs', c, undefined]]);
    e.query.removeRelation(a, 'refs');
    expect(edges()).toEqual([]);
  });

  it('link with replace leaves one edge between the ends; updateRelation moves one, keeping its id', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.query.write(a).link('refs', b, { info: 1 }).link('refs', b, { info: 2 });
    e.query.write(a).link('refs', b, { info: 3, replace: true });
    expect(edges()).toEqual([[a, 'refs', b, 3]]);
    const rel = e.query.findRelations()[0].id;
    e.query.updateRelation(rel, { targetEntity: c, info: 4 });
    expect(edges()).toEqual([[a, 'refs', c, 4]]);
    expect(e.query.qx.ids(b).linksTo('refs', undefined, false).ids()).toEqual([]);
    expect(e.query.qx.ids(c).linksTo('refs', undefined, false).ids()).toEqual([a]);
    e.query.updateRelation(rel, { sourceEntity: b, info: 5 });
    expect(edges()).toEqual([[b, 'refs', c, 5]]);
    expect(e.query.findRelations()[0].id).toBe(rel);
    expect(e.query.qx.ids(a).linksTo('refs').ids()).toEqual([]);
    expect(e.query.qx.ids(b).linksTo('refs').ids()).toEqual([c]);
    e.query.updateRelation('Relation-missing' as Id, { info: 6 });
  });

  it('link refuses a cycle within acyclicIn, and links both ways when symmetric', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.query.write(a).link('parent', b, { acyclicIn: ['parent'] });
    e.query.write(b).link('parent', c, { acyclicIn: ['parent'] });
    expect(() => e.query.write(c).link('parent', a, { acyclicIn: ['parent'] }))
      .toThrow('Cannot create a parent relation that would form a cycle');
    expect(() => e.query.write(c).link('child', a, { acyclicIn: ['parent', 'child'] }))
      .toThrow('Cannot create a child relation that would form a cycle within [parent, child]');
    expect(e.query.wouldCreateCycle(c, a, ['parent'])).toBe(true);
    expect(e.query.wouldCreateCycle(a, c, ['parent'])).toBe(false);
    e.query.write(c).link('parent', a);
    e.query.write(a).link('peer', c, { symmetric: true, info: 'p' });
    expect(edges().filter(([, kind]) => kind === 'peer')).toEqual([[a, 'peer', c, 'p'], [c, 'peer', a, 'p']]);
    e.query.linkSymmetric(b, c, 'peer');
    expect(e.query.qx.ids(c).linksTo('peer').ids()).toEqual([a, b]);
  });
});

describe('graph walks', () => {
  it('descendants, ancestors, roots, leaves, order, paths and common ancestors', () => {
    const [root, left, right, leaf] = [task('root'), task('left'), task('right'), task('leaf')];
    e.query.write(root).link('child', left).link('child', right);
    e.query.write(left).link('child', leaf);
    e.query.write(right).link('child', leaf);
    const other = e.query.create('Project').id();
    expect(e.query.descendants(root, 'child').sort()).toEqual([left, right, leaf].sort());
    expect(e.query.ancestors(leaf, 'child').sort()).toEqual([root, left, right].sort());
    expect(e.query.rootParent(leaf, 'child')).toBe(root);
    expect(e.query.leaves('child', 'Task')).toEqual([leaf]);
    // Without a type, every entity but the relations
    expect(e.query.leaves('child')).toEqual([leaf, other]);
    expect(e.query.topoSort([root], 'child')).toEqual([root, left, right, leaf]);
    expect(e.query.shortestPath(root, leaf, ['child'])).toEqual([root, left, leaf]);
    expect(e.query.shortestPath(leaf, root, ['child'])).toBeNull();
    expect(e.query.shortestPath(root, root, ['child'])).toEqual([root]);
    // Each path follows the first parent: leaf, left, root
    expect(e.query.lowestCommonAncestor(leaf, right, 'child')).toBe(root);
    expect(e.query.lowestCommonAncestor(leaf, left, 'child')).toBe(left);
    expect(e.query.lowestCommonAncestor(other, leaf, 'child')).toBeNull();
    e.query.write(leaf).link('child', root);
    expect(() => e.query.topoSort([root], 'child')).toThrow('Cycle detected');
  });
});

describe('roles', () => {
  it('grant, revoke, grantExclusive and withRole', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.query.write(a).grant('lead').grant('lead').grant('owner');
    expect(e.query.getRoles(a)).toEqual(['lead', 'owner']);
    e.query.write(b).grant('lead');
    expect(e.query.qx.all().withRole('lead').ids()).toEqual([a, b]);
    expect(e.admin.queryEntitiesByRole('lead')).toEqual([a, b]);
    e.query.write(c).grantExclusive('lead', e.query.qx('Task').ids());
    expect(e.query.qx.all().withRole('lead').ids()).toEqual([c]);
    // Revokes only among the entities given, and grants once
    e.query.write(b).grant('owner');
    e.query.write(a).grantExclusive('owner', [c]);
    expect(e.query.getRoles(a)).toEqual(['owner']);
    expect(e.query.getRoles(b)).toEqual(['owner']);
    e.query.write(a).revoke('owner').revoke('never');
    expect(e.query.getRoles(a)).toEqual([]);
    e.query.grantRole(b, 'free');
    e.query.grantRole(b, 'free');
    expect(e.query.getRoles(b)).toEqual(['owner', 'free', 'free']);
    e.query.revokeRole(b, 'free');
    expect(e.query.getRoles(b)).toEqual(['owner', 'free']);
  });
});
