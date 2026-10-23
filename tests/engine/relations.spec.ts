// The engine's contract: relations (links, their index and graph walks) and roles
import { beforeEach, describe, expect, it } from 'vitest';
import { EARS } from '../../src/index.ts';
import { engine, type Id } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

let e: EngineUnderTest;
const task = (title: string) => e.create('Task').set('title', title).id();
const edges = () => e.findRelations().map((r) => [r.source, r.kind, r.target, r.info]);

beforeEach(() => {
  e = engine();
});

describe('links', () => {
  it('link stores a Relation row with its details and indexes both ends', () => {
    const [a, b] = [task('A'), task('B')];
    e.write(a).link('blocks', b, { info: { weight: 2 } });
    const [rel] = e.findRelations();
    expect(rel).toEqual({ id: expect.stringMatching(/^Relation-/), source: a, target: b, kind: 'blocks', info: { weight: 2 } });
    expect(e.getRow(rel.id)?.relationDetails).toEqual({ source: a, target: b, kind: 'blocks', info: { weight: 2 } });
    expect(e.admin.getEntitiesOfType('Relation')).toEqual([rel.id]);
    expect(e.query.ids(a).out('blocks').ids()).toEqual([b]);
    expect(e.query.ids(b).in('blocks').ids()).toEqual([a]);
    expect(e.findRelations({ source: a }).map((r) => r.id)).toEqual([rel.id]);
    expect(e.findRelations({ target: b, kind: 'blocks' }).map((r) => r.id)).toEqual([rel.id]);
    expect(e.query('Task').linkedFrom('blocks', a).ids()).toEqual([b]);
    expect(e.query('Task').linkedTo('blocks', b).ids()).toEqual([a]);
    expect(e.query('Task').linkedWith(a).ids()).toEqual([b]);
    expect(e.getRelationStats('blocks')).toEqual({ total: 1, uniqueSources: 1, uniqueTargets: 1 });
    expect(e.getRelationStats('refs')).toEqual({ total: 0, uniqueSources: 0, uniqueTargets: 0 });
  });

  it('linkedTo and linkedFrom without an end, and unlinkedTo and unlinkedFrom, filter by having an edge of the kind at all', () => {
    const [root, child, grandchild, loose] = [task('root'), task('child'), task('grandchild'), task('loose')];
    e.write(root).link('child', child);
    e.write(child).link('child', grandchild);
    const tasks = () => e.query('Task');
    expect(tasks().linkedTo('child').ids()).toEqual([root, child]);
    expect(tasks().linkedFrom('child').ids()).toEqual([child, grandchild]);
    expect(tasks().unlinkedTo('child').ids()).toEqual([grandchild, loose]);
    // A tree's roots: what nothing links to
    expect(tasks().unlinkedFrom('child').ids()).toEqual([root, loose]);
    expect(tasks().unlinkedFrom('child', root).ids()).toEqual([root, grandchild, loose]);
    expect(tasks().unlinkedTo('child', grandchild).ids()).toEqual([root, grandchild, loose]);
    // An edge whose far end is in the trash is one this query can't reach, as out and in step around it
    e.write(root).trash();
    expect(tasks().unlinkedFrom('child').ids()).toEqual([child, loose]);
    expect(e.query.withTrashed('Task').unlinkedFrom('child').ids()).toEqual([root, loose]);
  });

  it('linking the same edge again changes nothing, keeping its id and its place; another info is another edge', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('blocks', b, { info: { x: 1 } }).link('blocks', c);
    const first = e.findRelations({ source: a, target: b })[0].id;
    e.write(a).link('blocks', b, { info: { x: 1 } });
    // No info given matches any edge between the ends
    e.write(a).link('blocks', b);
    expect(e.findRelations().map((r) => r.id)).toEqual([first, expect.any(String)]);
    expect(e.query.ids(a).out('blocks').ids()).toEqual([b, c]);
    e.write(a).link('blocks', b, { info: { x: 2 } });
    expect(e.findRelations({ source: a, target: b })).toHaveLength(2);
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
  });

  it('unlink, unlinkAll and destroying a relation by its id remove relations and empty index entries', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('blocks', b).link('blocks', c).link('refs', b).link('refs', c);
    const blocksAB = e.findRelations({ source: a, kind: 'blocks', target: b })[0].id;
    e.write(blocksAB).destroy();
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
    e.write(e.findRelations()[0].id).destroy();
    expect(edges()).toEqual([]);
    expect(e.getAllRelationKinds()).toEqual([]);
  });

  it('destroying a relation removes that one edge, leaving the other edges between its ends and the ends themselves', () => {
    const [a, b] = [task('A'), task('B')];
    e.write(a).link('blocks', b, { info: { handle: 1 } }).link('blocks', b, { info: { handle: 2 } });
    const [first] = e.findRelations();
    e.write(first.id).destroy();
    expect(edges()).toEqual([[a, 'blocks', b, { handle: 2 }]]);
    expect(e.query.ids([a, b]).ids()).toEqual([a, b]);
    expect(e.getRow(first.id)).toBeUndefined();
  });

  it('link with replace leaves one edge between the ends; updateRelation moves one, keeping its id', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('refs', b, { info: 1 }).link('refs', b, { info: 2 });
    e.write(a).link('refs', b, { info: 3, replace: true });
    expect(edges()).toEqual([[a, 'refs', b, 3]]);
    const rel = e.findRelations()[0].id;
    e.updateRelation(rel, { target: c, info: 4 });
    expect(edges()).toEqual([[a, 'refs', c, 4]]);
    expect(e.query.ids(b).in('refs').ids()).toEqual([]);
    expect(e.query.ids(c).in('refs').ids()).toEqual([a]);
    e.updateRelation(rel, { source: b, info: 5 });
    expect(edges()).toEqual([[b, 'refs', c, 5]]);
    expect(e.findRelations()[0].id).toBe(rel);
    expect(e.query.ids(a).out('refs').ids()).toEqual([]);
    expect(e.query.ids(b).out('refs').ids()).toEqual([c]);
    e.updateRelation('Relation-missing' as Id, { info: 6 });
  });

  it('link refuses a cycle within acyclicIn', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('parent', b, { acyclicIn: ['parent'] });
    e.write(b).link('parent', c, { acyclicIn: ['parent'] });
    expect(() => e.write(c).link('parent', a, { acyclicIn: ['parent'] }))
      .toThrow('Cannot create a parent relation that would form a cycle');
    expect(() => e.write(c).link('child', a, { acyclicIn: ['parent', 'child'] }))
      .toThrow('Cannot create a child relation that would form a cycle within [parent, child]');
    expect(e.wouldCreateCycle(c, a, ['parent'])).toBe(true);
    expect(e.wouldCreateCycle(a, c, ['parent'])).toBe(false);
    expect(e.wouldCreateCycle(a, a, ['parent'])).toBe(true);
  });
});

describe('relation kinds', () => {
  const unknown = { code: 'UNKNOWN_RELATION_KIND' };

  it('refuses a kind its creator did not register, before writing anything', () => {
    const [a, b] = [task('A'), task('B')];
    expect(() => e.write(a).link('blokcs', b)).toThrow(expect.objectContaining(unknown));
    expect(() => e.write(a).link('blocks', b, { acyclicIn: ['blokcs'] })).toThrow(expect.objectContaining(unknown));
    expect(() => e.write(a).unlink('blokcs')).toThrow(expect.objectContaining(unknown));
    expect(e.findRelations()).toEqual([]);
  });

  // A read naming a kind nothing registered would answer as if no entity had such a link, which reads as data
  it('refuses one in every read and walk that names a kind, rather than answering with nothing', () => {
    const [a, b] = [task('A'), task('B')];
    e.write(a).link('blocks', b);
    const reads: Array<[string, () => unknown]> = [
      ['out', () => e.query.ids(a).out('blokcs')],
      ['in', () => e.query.ids(b).in(['blocks', 'blokcs'])],
      ['outEdges', () => e.query.ids(a).outEdges('blokcs')],
      ['inEdges', () => e.query.ids(b).inEdges('blokcs')],
      ['linkedTo', () => e.query('Task').linkedTo('blokcs', b)],
      ['linkedFrom', () => e.query('Task').linkedFrom('blokcs', a)],
      ['findRelations', () => e.findRelations({ kind: 'blokcs' })],
      ['getRelationStats', () => e.getRelationStats('blokcs')],
      ['descendants', () => e.descendants(a, 'blokcs')],
      ['ancestors', () => e.ancestors(b, 'blokcs')],
      ['wouldCreateCycle', () => e.wouldCreateCycle(a, b, ['blokcs'])],
    ];
    for (const [via, read] of reads) expect(read, via).toThrow(expect.objectContaining({ ...unknown, message: expect.stringContaining(`${via}("blokcs")`) }));
  });

  it("loads a stored relation of any kind through the admin face, which is how data a pack's removal left is read", () => {
    const [a, b] = [task('A'), task('B')];
    e.admin.bulkLoadAttr('Relation-r1' as Id, EARS.AttrKind.RelationDetails, { source: a, target: b, kind: 'retired' });
    e.admin.addToIndex('retired', a, b, 'Relation-r1' as Id);
    expect(e.findRelations().map((r) => r.kind)).toEqual(['retired']);
  });
});

describe('hierarchy walks', () => {
  it('descendants and ancestors reach every entity through a kind once each; ancestors in a tree run parent to root', () => {
    const [root, left, right, leaf] = [task('root'), task('left'), task('right'), task('leaf')];
    e.write(root).link('child', left).link('child', right);
    e.write(left).link('child', leaf);
    e.write(right).link('child', leaf);
    expect(e.descendants(root, 'child').sort()).toEqual([left, right, leaf].sort());
    expect(e.ancestors(leaf, 'child').sort()).toEqual([root, left, right].sort());
    expect(e.descendants(leaf, 'child')).toEqual([]);
    expect(e.ancestors(left, 'child')).toEqual([root]);

    const [a, b, c] = [task('a'), task('b'), task('c')];
    e.write(a).link('child', b);
    e.write(b).link('child', c);
    expect(e.ancestors(c, 'child')).toEqual([b, a]);
    // A cycle in the data ends the walk rather than the stack
    e.write(c).link('child', a);
    expect(e.descendants(a, 'child')).toEqual([b, c]);
    expect(e.ancestors(a, 'child')).toEqual([c, b]);
  });
});

describe('declared roles', () => {
  const unknown = { code: 'UNKNOWN_ROLE' };

  it('refuses a role its creator did not register, in a role write and a query by role', () => {
    const [a, b] = [task('A'), task('B')];
    const uses: Array<[string, () => unknown]> = [
      ['grant', () => e.write(a).grant('leed')],
      ['revoke', () => e.write(a).revoke('leed')],
      ['grantExclusive', () => e.write(a).grantExclusive('leed', [b])],
      ['withRole', () => e.query.all().withRole('leed')],
    ];
    for (const [via, use] of uses) expect(use, via).toThrow(expect.objectContaining({ ...unknown, message: expect.stringContaining(`${via}("leed")`) }));
    expect(e.getRoles(a)).toEqual([]);
  });

  it("loads a stored role of any kind through the admin face, which is how data a pack's removal left is read", () => {
    const a = task('A');
    e.admin.putAttr(a, 'role', ['retired']);
    expect(e.getRoles(a)).toEqual(['retired']);
    expect(e.getAllRoles()).toEqual(['retired']);
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
    e.write(a).revoke('owner').revoke('member');
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
