// The engine's contract: relations (links, their index and graph walks) and roles
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { engine, type Id } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

let e: EngineUnderTest;
const task = (title: string) => e.query.tx('Task').put('title', title).id();
const edges = () => e.query.findRelations().map((r) => [r.sourceEntity, r.relationType, r.targetEntity, r.info]);

beforeEach(() => {
  e = engine();
});

describe('links', () => {
  it('link stores a Relation row with its details and indexes both ends', () => {
    const [a, b] = [task('A'), task('B')];
    e.query.tx(a).link('blocks', b, { weight: 2 });
    const [rel] = e.query.findRelations();
    expect(rel).toEqual({ id: expect.stringMatching(/^Relation-/), sourceEntity: a, targetEntity: b, relationType: 'blocks', info: { weight: 2 } });
    expect(e.query.getAttr(rel.id, 'relationDetails')).toEqual({ sourceEntity: a, targetEntity: b, relationType: 'blocks', info: { weight: 2 } });
    expect(e.query.getEntitiesOfType('Relation')).toEqual([rel.id]);
    expect(e.query.qx(a).linksTo('blocks').ids()).toEqual([b]);
    expect(e.query.qx(b).linksTo('blocks', undefined, false).ids()).toEqual([a]);
    expect(e.query.qx(a).edgeIds()).toEqual([rel.id]);
    expect(e.query.qx(b).edgeIds('blocks', false)).toEqual([rel.id]);
    expect(e.query.qx(a).edgeIds('missing')).toEqual([]);
    expect(e.query.queryEntitiesByRelationTo('blocks', a, true)).toEqual([b]);
    expect(e.query.queryEntitiesByRelationTo('blocks', b)).toEqual([a]);
    expect(e.query.queryEntitiesInRelationTo(a)).toEqual([b]);
    expect(e.query.getRelationStats('blocks')).toEqual({ total: 1, uniqueSources: 1, uniqueTargets: 1 });
    expect(e.query.getRelationStats('none')).toEqual({ total: 0, uniqueSources: 0, uniqueTargets: 0 });
    expect(e.admin.relationIndex).toEqual({ blocks: { bySource: { [a]: [rel.id] }, byTarget: { [b]: [rel.id] } } });
  });

  it('a duplicate link reuses the relation unless its info differs', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const [a, b] = [task('A'), task('B')];
    e.query.tx(a).link('blocks', b, { x: 1 });
    e.query.tx(a).link('blocks', b, { x: 1 });
    e.query.tx(a).link('blocks', b);
    expect(e.query.findRelations()).toHaveLength(1);
    expect(warn).toHaveBeenCalledTimes(2);
    e.query.tx(a).link('blocks', b, { x: 2 });
    expect(e.query.findRelations()).toHaveLength(2);
    warn.mockRestore();
  });

  it('refuses a self link', () => {
    const a = task('A');
    expect(() => e.query.tx(a).link('blocks', a)).toThrow('tx.link(): source and target cannot be the same');
    expect(() => e.query.tx(a).linkOne('blocks', a)).toThrow('source and target cannot be the same');
    expect(() => e.query.linkSymmetric(a, a, 'peer')).toThrow('Self-link refused');
  });

  it('unlink, unlinkIf, unlinkWhere and removeRelationById remove relations and empty index entries', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.query.tx(a).link('blocks', b).link('blocks', c).link('refs', b).link('refs', c);
    const blocksAB = e.query.findRelations({ sourceEntity: a, relationType: 'blocks', targetEntity: b })[0].id;
    e.query.tx(a).unlink(blocksAB);
    expect(edges()).toEqual([[a, 'blocks', c, undefined], [a, 'refs', b, undefined], [a, 'refs', c, undefined]]);
    e.query.tx(a).unlinkIf('refs', b);
    expect(edges()).toEqual([[a, 'blocks', c, undefined], [a, 'refs', c, undefined]]);
    e.query.tx(a).unlinkWhere({ target: c });
    expect(edges()).toEqual([]);
    expect(e.query.getAllRelationKinds()).toEqual([]);
    // A removed relation's id stays in the type index, with no attributes
    expect(e.query.getEntitiesOfType('Relation')).toHaveLength(4);
    expect(e.query.qx('Relation').pickAll().map((row) => Object.keys(row))).toEqual([['id'], ['id'], ['id'], ['id']]);

    e.query.tx(a).link('blocks', b).link('blocks', c).link('refs', c);
    e.query.tx(a).unlinkIf('blocks');
    expect(edges()).toEqual([[a, 'refs', c, undefined]]);
    e.query.tx(a).unlinkWhere();
    expect(edges()).toEqual([]);

    e.query.tx(b).link('refs', c);
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

  it('linkOne replaces the relation between the same ends; patchLink and relPatch move one', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.query.tx(a).link('refs', b, 1).link('refs', b, 2);
    e.query.tx(a).linkOne('refs', b, 3);
    expect(edges()).toEqual([[a, 'refs', b, 3]]);
    e.query.tx(a).patchLink('refs', b, { newTarget: c, newInfo: 4 });
    expect(edges()).toEqual([[a, 'refs', c, 4]]);
    expect(e.query.qx(b).linksTo('refs', undefined, false).ids()).toEqual([]);
    expect(e.query.qx(c).linksTo('refs', undefined, false).ids()).toEqual([a]);
    e.query.tx(a).patchLink('refs', b, { newTarget: b });
    const rel = e.query.findRelations()[0].id;
    e.query.tx(a).relPatch(rel, { sourceEntity: b, info: 5 });
    expect(edges()).toEqual([[b, 'refs', c, 5]]);
    expect(e.query.qx(a).linksTo('refs').ids()).toEqual([]);
    expect(e.query.qx(b).linksTo('refs').ids()).toEqual([c]);
    e.query.tx(a).relPatch('Relation-missing' as Id, { info: 6 });
  });

  it('safeLink refuses cycles within its group and links symmetrically when asked', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.query.tx(a).safeLink('parent', b, { acyclicGroup: ['parent'] });
    e.query.tx(b).safeLink('parent', c, { acyclicGroup: ['parent'] });
    expect(() => e.query.tx(c).safeLink('parent', a, { acyclicGroup: ['parent'] }))
      .toThrow('Cannot create a parent relation that would form a cycle');
    expect(() => e.query.tx(c).safeLink('child', a, { acyclicGroup: ['parent', 'child'] }))
      .toThrow('Cannot create a child relation that would form a cycle within [parent, child]');
    expect(e.query.wouldCreateCycle(c, a, ['parent'])).toBe(true);
    expect(e.query.wouldCreateCycle(a, c, ['parent'])).toBe(false);
    e.query.tx(c).safeLink('parent', a);
    e.query.tx(a).safeLink('peer', c, { symmetric: true, info: 'p' });
    expect(edges().filter(([, kind]) => kind === 'peer')).toEqual([[a, 'peer', c, 'p'], [c, 'peer', a, 'p']]);
    e.query.linkSymmetric(b, c, 'peer');
    expect(e.query.qx(c).linksTo('peer').ids()).toEqual([a, b]);
  });
});

describe('graph walks', () => {
  it('descendants, ancestors, roots, leaves, order, paths and common ancestors', () => {
    const [root, left, right, leaf] = [task('root'), task('left'), task('right'), task('leaf')];
    e.query.tx(root).link('child', left).link('child', right);
    e.query.tx(left).link('child', leaf);
    e.query.tx(right).link('child', leaf);
    const other = e.query.tx('Project').id();
    expect(e.query.descendants(root, 'child').sort()).toEqual([left, right, leaf].sort());
    expect(e.query.ancestors(leaf, 'child').sort()).toEqual([root, left, right].sort());
    expect(e.query.rootParent(leaf, 'child')).toBe(root);
    expect(e.query.leaves('child', 'Task')).toEqual([leaf]);
    // Without a type, relation rows count as leaves too
    expect(e.query.leaves('child')).toEqual([leaf, ...e.query.getEntitiesOfType('Relation'), other]);
    expect(e.query.topoSort([root], 'child')).toEqual([root, left, right, leaf]);
    expect(e.query.shortestPath(root, leaf, ['child'])).toEqual([root, left, leaf]);
    expect(e.query.shortestPath(leaf, root, ['child'])).toBeNull();
    expect(e.query.shortestPath(root, root, ['child'])).toEqual([root]);
    // Each path follows the first parent: leaf, left, root
    expect(e.query.lowestCommonAncestor(leaf, right, 'child')).toBe(root);
    expect(e.query.lowestCommonAncestor(leaf, left, 'child')).toBe(left);
    expect(e.query.lowestCommonAncestor(other, leaf, 'child')).toBeNull();
    e.query.tx(leaf).link('child', root);
    expect(() => e.query.topoSort([root], 'child')).toThrow('Cycle detected');
  });
});

describe('roles', () => {
  it('grant, revoke, ensure and withRole', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.query.tx(a).grant('lead').grant('lead').grant('owner');
    expect(e.query.getRoles(a)).toEqual(['lead', 'owner']);
    e.query.tx(b).grant('lead');
    expect(e.query.qx().withRole('lead').ids()).toEqual([a, b]);
    expect(e.admin.queryEntitiesByRole('lead')).toEqual([a, b]);
    e.query.tx(c).ensure('lead');
    expect(e.query.qx().withRole('lead').ids()).toEqual([c]);
    // ensure revokes only within its scope, and grants again
    e.query.tx(a).ensure('owner', [b]);
    expect(e.query.getRoles(a)).toEqual(['owner', 'owner']);
    e.query.tx(a).revoke('owner').revoke('never');
    expect(e.query.getRoles(a)).toEqual(['owner']);
    e.query.tx(a).revoke('owner');
    expect(e.query.getRoles(a)).toEqual([]);
    e.query.grantRole(b, 'free');
    e.query.grantRole(b, 'free');
    expect(e.query.getRoles(b)).toEqual(['free', 'free']);
    e.query.revokeRole(b, 'free');
    expect(e.query.getRoles(b)).toEqual(['free']);
  });
});
