// The engine's contract: relations (links, their index and graph walks) and roles
import { beforeEach, describe, expect, it } from 'vitest';
import { engine, type RelId } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

let e: EngineUnderTest;
const task = (title: string) => e.create('Task').set('title', title).id();
const edges = () => e.findRelations().map((r) => [r.source, r.kind, r.target, r.info]);

beforeEach(() => {
  e = engine();
});

describe('links', () => {
  it('link stores a relation row, which is no entity, and indexes both ends', () => {
    const [a, b] = [task('A'), task('B')];
    e.write(a).link('blocks', b, { info: { weight: 2 } });
    const [rel] = e.findRelations();
    expect(rel).toEqual({ id: expect.stringMatching(/^Relation-/), source: a, target: b, kind: 'blocks', info: { weight: 2 } });
    // A relation is read through findRelations: no entity has its id, so no read of entities reaches it
    expect(e.getRow(rel.id)).toBeUndefined();
    expect(e.query.ids(rel.id).ids()).toEqual([]);
    expect(e.query.all().ids()).toEqual([a, b]);
    expect(e.admin.getAllEntities()).toEqual([a, b]);
    expect(e.getSchemaStats().entities).toEqual({ Task: 2 });
    // Nor does write: the compiler refuses a relation's id there, and given one untyped, the refusal names where it goes
    // @ts-expect-error a relation's id is no entity's
    expect(() => e.write(rel.id)).toThrow(expect.objectContaining({ code: 'NO_SUCH_ENTITY', message: expect.stringContaining('removeRelation(id) removes it') }));
    expect(e.getRelation(rel.id)).toEqual(rel);
    expect(e.getRelation('Relation-missing' as RelId)).toBeUndefined();
    expect(e.query.ids(a).out('blocks').ids()).toEqual([b]);
    expect(e.query.ids(b).in('blocks').ids()).toEqual([a]);
    expect(e.findRelations({ source: a }).map((r) => r.id)).toEqual([rel.id]);
    expect(e.findRelations({ target: b, kind: 'blocks' }).map((r) => r.id)).toEqual([rel.id]);
    expect(e.query('Task').linkedFrom('blocks', a).ids()).toEqual([b]);
    expect(e.query('Task').linkedTo('blocks', b).ids()).toEqual([a]);
    expect(e.query('Task').linkedWith(a).ids()).toEqual([b]);
    expect(e.getSchemaStats().relations.blocks).toEqual({ total: 1, uniqueSources: 1, uniqueTargets: 1 });
    expect(e.getSchemaStats().relations.refs).toBeUndefined();
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

  it('an info with the same fields in another order is the same edge', () => {
    const [a, b] = [task('A'), task('B')];
    e.write(a).link('blocks', b, { info: { x: 1, at: { y: [1, 2], z: 'z' } } });
    e.write(a).link('blocks', b, { info: { at: { z: 'z', y: [1, 2] }, x: 1 } });
    expect(e.findRelations()).toHaveLength(1);
    // A list's order is its value
    e.write(a).link('blocks', b, { info: { x: 1, at: { y: [2, 1], z: 'z' } } });
    expect(e.findRelations()).toHaveLength(2);
  });

  it('refuses options it does not know, which is info passed where the options go', () => {
    const [a, b] = [task('A'), task('B')];
    const info: any = { weight: 2 };
    expect(() => e.write(a).link('blocks', b, info)).toThrow("unknown option weight; an edge's info goes in { info }");
    expect(e.findRelations()).toEqual([]);
  });

  it('refuses a self link', () => {
    const a = task('A');
    expect(() => e.write(a).link('blocks', a)).toThrow(expect.objectContaining({ code: 'SELF_LINK' }));
    expect(e.findRelations()).toEqual([]);
  });

  it('unlink, unlinkAll and removeRelation remove relations and empty index entries', () => {
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('blocks', b).link('blocks', c).link('refs', b).link('refs', c);
    const blocksAB = e.findRelations({ source: a, kind: 'blocks', target: b })[0].id;
    e.removeRelation(blocksAB);
    expect(edges()).toEqual([[a, 'blocks', c, undefined], [a, 'refs', b, undefined], [a, 'refs', c, undefined]]);
    e.write(a).unlink('refs', b);
    expect(edges()).toEqual([[a, 'blocks', c, undefined], [a, 'refs', c, undefined]]);
    e.write(a).unlink('blocks', c).unlink('refs', c);
    expect(edges()).toEqual([]);
    expect(Object.keys(e.getSchemaStats().relations)).toEqual([]);

    e.write(a).link('blocks', b).link('blocks', c).link('refs', c);
    e.write(a).unlink('blocks');
    expect(edges()).toEqual([[a, 'refs', c, undefined]]);
    e.write(c).link('refs', a);
    e.write(a).unlinkAll();
    // Only the edges from it
    expect(edges()).toEqual([[c, 'refs', a, undefined]]);
    e.write(c).unlinkAll();

    e.write(b).link('refs', c);
    e.removeRelation(e.findRelations()[0].id);
    expect(edges()).toEqual([]);
    expect(Object.keys(e.getSchemaStats().relations)).toEqual([]);
  });

  it('removeRelation removes that one edge, leaving the other edges between its ends and the ends themselves, and refuses an id no relation has', () => {
    const [a, b] = [task('A'), task('B')];
    e.write(a).link('blocks', b, { info: { handle: 1 } }).link('blocks', b, { info: { handle: 2 } });
    const [first] = e.findRelations();
    e.removeRelation(first.id);
    expect(edges()).toEqual([[a, 'blocks', b, { handle: 2 }]]);
    expect(e.query.ids([a, b]).ids()).toEqual([a, b]);
    expect(e.getRelation(first.id)).toBeUndefined();
    expect(() => e.removeRelation(first.id)).toThrow(expect.objectContaining({ code: 'NO_SUCH_RELATION' }));
  });

  it('a kind declared unique keeps one edge between two ends, which linking again replaces the info of; updateRelation moves one, keeping its id, and refuses an id no relation has', () => {
    e = engine({ relationRules: { refs: { unique: true } } });
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('refs', b, { info: 1 }).link('refs', b, { info: 2 });
    e.write(a).link('refs', b, { info: 3 });
    expect(edges()).toEqual([[a, 'refs', b, 3]]);
    e.write(a).link('refs', b);
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
    expect(() => e.updateRelation('Relation-missing' as RelId, { info: 6 })).toThrow(expect.objectContaining({ code: 'NO_SUCH_RELATION' }));
  });

  it('a kind declared acyclic refuses a link that would close a cycle through it', () => {
    e = engine({ relationRules: { parent: { acyclic: true } } });
    const [a, b, c] = [task('A'), task('B'), task('C')];
    e.write(a).link('parent', b);
    e.write(b).link('parent', c);
    expect(() => e.write(c).link('parent', a)).toThrow(expect.objectContaining({ code: 'CYCLE' }));
    // A kind not declared acyclic takes the same edge
    e.write(c).link('child', a);
    expect(e.findRelations({ kind: 'parent' as never })).toHaveLength(2);
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
      ['descendants', () => e.query.ids(a).descendants('blokcs')],
      ['ancestors', () => e.query.ids(b).ancestors('blokcs')],
      ['both', () => e.query.ids(a).both('blokcs')],
      ['pathTo', () => e.query.ids(a).pathTo(b, 'blokcs')],
      ['degrees', () => e.query.ids(a).degrees('blokcs')],
      ['wouldCreateCycle', () => e.wouldCreateCycle(a, b, ['blokcs'])],
    ];
    for (const [via, read] of reads) expect(read, via).toThrow(expect.objectContaining({ ...unknown, message: expect.stringContaining(`${via}("blokcs")`) }));
  });

  it("loads a stored relation of any kind through the admin face, which is how data a pack's removal left is read", () => {
    const [a, b] = [task('A'), task('B')];
    e.admin.bulkLoadRelation('Relation-r1' as RelId, 'retired', a, b);
    expect(e.findRelations().map((r) => r.kind)).toEqual(['retired']);
  });
});

describe('hierarchy walks', () => {
  it('descendants and ancestors reach every entity through a kind once each; ancestors in a tree run parent to root', () => {
    const [root, left, right, leaf] = [task('root'), task('left'), task('right'), task('leaf')];
    e.write(root).link('child', left).link('child', right);
    e.write(left).link('child', leaf);
    e.write(right).link('child', leaf);
    expect(e.query.ids(root).descendants('child').ids().sort()).toEqual([left, right, leaf].sort());
    expect(e.query.ids(leaf).ancestors('child').ids().sort()).toEqual([root, left, right].sort());
    expect(e.query.ids(leaf).descendants('child').ids()).toEqual([]);
    expect(e.query.ids(left).ancestors('child').ids()).toEqual([root]);

    const [a, b, c] = [task('a'), task('b'), task('c')];
    e.write(a).link('child', b);
    e.write(b).link('child', c);
    expect(e.query.ids(c).ancestors('child').ids()).toEqual([b, a]);
    // A cycle in the data ends the walk rather than the stack
    e.write(c).link('child', a);
    expect(e.query.ids(a).descendants('child').ids()).toEqual([b, c]);
    expect(e.query.ids(a).ancestors('child').ids()).toEqual([c, b]);
  });

  it('walks nearest first, keeps the reached entities of a type, and goes only where the query could reach', () => {
    const root = e.create('Project').id();
    const [mid, deep] = [task('mid'), task('deep')];
    const leaf = e.create('Project').id();
    e.write(root).link('child', mid);
    e.write(mid).link('child', deep);
    e.write(deep).link('child', leaf);
    expect(e.query.ids(root).descendants('child').ids()).toEqual([mid, deep, leaf]);
    // A type keeps what it names and walks through the rest
    expect(e.query.ids(root).descendants('child', 'Project').ids()).toEqual([leaf]);
    expect(e.query.ids(leaf).ancestors('child', 'Task').ids()).toEqual([deep, mid]);

    // A trashed entity ends the walk there, unless the query reaches the trash
    e.write(mid).trash();
    expect(e.query.ids(leaf).ancestors('child').ids()).toEqual([deep]);
    expect(e.query.withTrashed.ids(leaf).ancestors('child').ids()).toEqual([deep, mid, root]);
  });

  it('stops at a depth, a count of edges taken as limit takes one', () => {
    const [a, b, c, d] = [task('a'), task('b'), task('c'), task('d')];
    e.write(a).link('child', b);
    e.write(b).link('child', c);
    e.write(c).link('child', d);
    expect(e.query.ids(a).descendants('child', undefined, { depth: 1 }).ids()).toEqual([b]);
    expect(e.query.ids(a).descendants('child', undefined, { depth: 2.9 }).ids()).toEqual([b, c]);
    expect(e.query.ids(a).descendants('child', undefined, { depth: 0 }).ids()).toEqual([]);
    expect(e.query.ids(a).descendants('child', undefined, { depth: -1 }).ids()).toEqual([]);
    expect(e.query.ids(a).descendants('child', undefined, { depth: NaN }).ids()).toEqual([]);
    expect(e.query.ids(a).descendants('child', undefined, { depth: Infinity }).ids()).toEqual([b, c, d]);
    expect(e.query.ids(d).ancestors('child', 'Task', { depth: 2 }).ids()).toEqual([c, b]);
  });
});

describe('graph walks', () => {
  it('both follows edges out and in, each far end once', () => {
    const [a, b, c] = [task('a'), task('b'), task('c')];
    e.write(a).link('refs', b);
    e.write(c).link('refs', a);
    e.write(b).link('refs', a);
    expect(e.query.ids(a).both('refs').ids()).toEqual([b, c]);
    expect(e.query.ids(a).both('refs', 'Task').pick(['title'])).toEqual([{ id: b, title: 'b' }, { id: c, title: 'c' }]);
  });

  it('pathTo answers a shortest path from the set, following edges the way it is told, or none', () => {
    const [a, b, c, d, x] = [task('a'), task('b'), task('c'), task('d'), task('x')];
    e.write(a).link('refs', b);
    e.write(b).link('refs', c);
    e.write(c).link('refs', d);
    e.write(a).link('blocks', d);
    expect(e.query.ids(a).pathTo(d, 'refs').ids()).toEqual([a, b, c, d]);
    expect(e.query.ids(a).pathTo(d, ['refs', 'blocks']).ids()).toEqual([a, d]);
    expect(e.query.ids(a).pathTo(d, 'refs', { depth: 2 }).ids()).toEqual([]);
    expect(e.query.ids(a).pathTo(x, 'refs').ids()).toEqual([]);
    expect(e.query.ids(d).pathTo(a, 'refs').ids()).toEqual([]);
    expect(e.query.ids(d).pathTo(a, 'refs', { direction: 'in' }).ids()).toEqual([d, c, b, a]);
    expect(e.query.ids(c).pathTo(a, 'refs', { direction: 'both' }).pick(['title']).map((r) => r.title)).toEqual(['c', 'b', 'a']);
    // A trashed entity is walked around, unless the query reaches the trash
    e.write(b).trash();
    expect(e.query.ids(a).pathTo(c, 'refs').ids()).toEqual([]);
    expect(e.query.withTrashed.ids(a).pathTo(c, 'refs').ids()).toEqual([a, b, c]);
  });

  it("degrees counts each entity's edges as outEdges and inEdges answer them", () => {
    const [a, b, c] = [task('a'), task('b'), task('c')];
    const project = e.create('Project').id();
    e.write(a).link('refs', b).link('refs', c).link('refs', project);
    e.write(b).link('refs', a);
    expect(e.query.ids([a, b, c]).degrees('refs')).toEqual(new Map([[a, 3], [b, 1], [c, 0]]));
    expect(e.query.ids([a, b, c]).degrees('refs', undefined, { direction: 'in' })).toEqual(new Map([[a, 1], [b, 1], [c, 1]]));
    expect(e.query.ids([a, b, c]).degrees('refs', 'Task', { direction: 'both' })).toEqual(new Map([[a, 3], [b, 2], [c, 1]]));
    expect(e.query.ids(a).degrees('refs').get(a)).toBe(e.query.ids(a).outEdges('refs').length);
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
    expect(e.getSchemaStats().roles).toEqual({ retired: 1 });
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
