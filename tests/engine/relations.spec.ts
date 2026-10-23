// The engine's contract: relations (links, their index and graph walks) and roles
import { beforeEach, describe, expect, it } from 'vitest';
import { engine, type Id } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

let e: EngineUnderTest;
const task = (title: string) => e.create('Task').set('title', title).id();
const edges = () => e.findRelations().map((r) => [r.sourceEntity, r.relationType, r.targetEntity, r.info]);

beforeEach(() => {
  e = engine();
});

describe('links', () => {
  it('link stores a Relation row with its details and indexes both ends', () => {
    const [a, b] = [task('A'), task('B')];
    e.write(a).link('blocks', b, { info: { weight: 2 } });
    const [rel] = e.findRelations();
    expect(rel).toEqual({ id: expect.stringMatching(/^Relation-/), sourceEntity: a, targetEntity: b, relationType: 'blocks', info: { weight: 2 } });
    expect(e.getAttr(rel.id, 'relationDetails')).toEqual({ sourceEntity: a, targetEntity: b, relationType: 'blocks', info: { weight: 2 } });
    expect(e.admin.getEntitiesOfType('Relation')).toEqual([rel.id]);
    expect(e.query.ids(a).linksTo('blocks').ids()).toEqual([b]);
    expect(e.query.ids(b).linksTo('blocks', undefined, false).ids()).toEqual([a]);
    expect(e.query.ids(a).edgeIds()).toEqual([rel.id]);
    expect(e.query.ids(b).edgeIds('blocks', false)).toEqual([rel.id]);
    expect(e.query.ids(a).edgeIds('missing')).toEqual([]);
    expect(e.query('Task').related('blocks', a, true).ids()).toEqual([b]);
    expect(e.query('Task').related('blocks', b).ids()).toEqual([a]);
    expect(e.query('Task').relatedTo(a).ids()).toEqual([b]);
    expect(e.getRelationStats('blocks')).toEqual({ total: 1, uniqueSources: 1, uniqueTargets: 1 });
    expect(e.getRelationStats('none')).toEqual({ total: 0, uniqueSources: 0, uniqueTargets: 0 });
    expect(e.admin.relationIndex).toEqual({ blocks: { bySource: { [a]: [rel.id] }, byTarget: { [b]: [rel.id] } } });
  });

  it('linking the same edge again changes nothing, keeping its id and its place; another info is another edge', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('blocks', b, { info: { x: 1 } }).link('blocks', c);
    const first = e.findRelations({ sourceEntity: a, targetEntity: b })[0].id;
    e.write(a).link('blocks', b, { info: { x: 1 } });
    // No info given matches any edge between the ends
    e.write(a).link('blocks', b);
    expect(e.findRelations().map((r) => r.id)).toEqual([first, expect.any(String)]);
    expect(e.query.ids(a).linksTo('blocks').ids()).toEqual([b, c]);
    e.write(a).link('blocks', b, { info: { x: 2 } });
    expect(e.findRelations({ sourceEntity: a, targetEntity: b })).toHaveLength(2);
  });

  it('refuses options it does not know, which is info passed where the options go', () => {
    const [a, b] = [task('A'), task('B')];
    const info: any = { weight: 2 };
    expect(() => e.write(a).link('blocks', b, info)).toThrow("unknown option weight; an edge's info goes in { info }");
    expect(e.findRelations()).toEqual([]);
  });

  it('refuses a self link', () => {
    const a = task('A');
    expect(() => e.write(a).link('blocks', a)).toThrow("an entity can't be linked to itself");
    expect(() => e.write(a).link('blocks', a, { replace: true })).toThrow("can't be linked to itself");
    expect(() => e.linkSymmetric(a, a, 'peer')).toThrow('Self-link refused');
  });

  it('unlink, unlinkAll and removeRelationById remove relations and empty index entries', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('blocks', b).link('blocks', c).link('refs', b).link('refs', c);
    const blocksAB = e.findRelations({ sourceEntity: a, relationType: 'blocks', targetEntity: b })[0].id;
    e.removeRelationById(blocksAB);
    expect(edges()).toEqual([[a, 'blocks', c, undefined], [a, 'refs', b, undefined], [a, 'refs', c, undefined]]);
    e.write(a).unlink('refs', b);
    expect(edges()).toEqual([[a, 'blocks', c, undefined], [a, 'refs', c, undefined]]);
    e.write(a).unlink('blocks', c).unlink('refs', c);
    expect(edges()).toEqual([]);
    expect(e.getAllRelationKinds()).toEqual([]);
    // A relation is its details: a removed one leaves no entity behind
    expect(e.admin.getEntitiesOfType('Relation')).toEqual([]);

    e.write(a).link('blocks', b).link('blocks', c).link('refs', c);
    e.write(a).unlink('blocks');
    expect(edges()).toEqual([[a, 'refs', c, undefined]]);
    e.write(c).link('refs', a);
    e.write(a).unlinkAll();
    // Only the edges from it
    expect(edges()).toEqual([[c, 'refs', a, undefined]]);
    e.write(c).unlinkAll();

    e.write(b).link('refs', c);
    e.removeRelationById(e.findRelations()[0].id);
    expect(edges()).toEqual([]);
    expect(e.admin.relationIndex).toEqual({});
  });

  it('link with replace leaves one edge between the ends; updateRelation moves one, keeping its id', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('refs', b, { info: 1 }).link('refs', b, { info: 2 });
    e.write(a).link('refs', b, { info: 3, replace: true });
    expect(edges()).toEqual([[a, 'refs', b, 3]]);
    const rel = e.findRelations()[0].id;
    e.updateRelation(rel, { targetEntity: c, info: 4 });
    expect(edges()).toEqual([[a, 'refs', c, 4]]);
    expect(e.query.ids(b).linksTo('refs', undefined, false).ids()).toEqual([]);
    expect(e.query.ids(c).linksTo('refs', undefined, false).ids()).toEqual([a]);
    e.updateRelation(rel, { sourceEntity: b, info: 5 });
    expect(edges()).toEqual([[b, 'refs', c, 5]]);
    expect(e.findRelations()[0].id).toBe(rel);
    expect(e.query.ids(a).linksTo('refs').ids()).toEqual([]);
    expect(e.query.ids(b).linksTo('refs').ids()).toEqual([c]);
    e.updateRelation('Relation-missing' as Id, { info: 6 });
  });

  it('link refuses a cycle within acyclicIn, and links both ways when symmetric', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('parent', b, { acyclicIn: ['parent'] });
    e.write(b).link('parent', c, { acyclicIn: ['parent'] });
    expect(() => e.write(c).link('parent', a, { acyclicIn: ['parent'] }))
      .toThrow('Cannot create a parent relation that would form a cycle');
    expect(() => e.write(c).link('child', a, { acyclicIn: ['parent', 'child'] }))
      .toThrow('Cannot create a child relation that would form a cycle within [parent, child]');
    expect(e.wouldCreateCycle(c, a, ['parent'])).toBe(true);
    expect(e.wouldCreateCycle(a, c, ['parent'])).toBe(false);
    e.write(c).link('parent', a);
    e.write(a).link('peer', c, { symmetric: true, info: 'p' });
    expect(edges().filter(([, kind]) => kind === 'peer')).toEqual([[a, 'peer', c, 'p'], [c, 'peer', a, 'p']]);
    e.linkSymmetric(b, c, 'peer');
    expect(e.query.ids(c).linksTo('peer').ids()).toEqual([a, b]);
  });
});

describe('graph walks', () => {
  it('descendants, ancestors, roots, leaves, order, paths and common ancestors', () => {
    const [root, left, right, leaf] = [task('root'), task('left'), task('right'), task('leaf')];
    e.write(root).link('child', left).link('child', right);
    e.write(left).link('child', leaf);
    e.write(right).link('child', leaf);
    const other = e.create('Project').id();
    expect(e.descendants(root, 'child').sort()).toEqual([left, right, leaf].sort());
    expect(e.ancestors(leaf, 'child').sort()).toEqual([root, left, right].sort());
    expect(e.rootParent(leaf, 'child')).toBe(root);
    expect(e.leaves('child', 'Task')).toEqual([leaf]);
    // Without a type, every entity but the relations
    expect(e.leaves('child')).toEqual([leaf, other]);
    expect(e.topoSort([root], 'child')).toEqual([root, left, right, leaf]);
    expect(e.shortestPath(root, leaf, ['child'])).toEqual([root, left, leaf]);
    expect(e.shortestPath(leaf, root, ['child'])).toBeNull();
    expect(e.shortestPath(root, root, ['child'])).toEqual([root]);
    // Each path follows the first parent: leaf, left, root
    expect(e.lowestCommonAncestor(leaf, right, 'child')).toBe(root);
    expect(e.lowestCommonAncestor(leaf, left, 'child')).toBe(left);
    expect(e.lowestCommonAncestor(other, leaf, 'child')).toBeNull();
    e.write(leaf).link('child', root);
    expect(() => e.topoSort([root], 'child')).toThrow('Cycle detected');
  });
});

describe('roles', () => {
  it('grant, revoke, grantExclusive and withRole', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).grant('lead').grant('lead').grant('owner');
    expect(e.getRoles(a)).toEqual(['lead', 'owner']);
    e.write(b).grant('lead');
    expect(e.query.all().withRole('lead').ids()).toEqual([a, b]);
    e.write(c).grantExclusive('lead', e.query('Task').ids());
    expect(e.query.all().withRole('lead').ids()).toEqual([c]);
    // Revokes only among the entities given, and grants once
    e.write(b).grant('owner');
    e.write(a).grantExclusive('owner', [c]);
    expect(e.getRoles(a)).toEqual(['owner']);
    expect(e.getRoles(b)).toEqual(['owner']);
    e.write(a).revoke('owner').revoke('never');
    expect(e.getRoles(a)).toEqual([]);
    // A role is held once: granting it again changes nothing, and revoking the last leaves no role attribute
    e.write(b).grant('free');
    e.write(b).grant('free');
    expect(e.getRoles(b)).toEqual(['owner', 'free']);
    e.write(b).revoke('free');
    e.write(b).revoke('owner');
    expect(e.getRoles(b)).toEqual([]);
    expect(e.getRow(b)).not.toHaveProperty('role');
  });
});
