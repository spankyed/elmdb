// Every member of the engine's faces belongs to one family, and a family has one rule (`CLAUDE.md`, Vocabulary). A
// member added without a family fails here, which is the point at which to ask whether it completes a family or
// starts a second name for one. The members are read off a real engine, so the table cannot describe a face that
// isn't there.
import { describe, expect, it } from 'vitest';
import { freshEngine } from './engine/engine-under-test.ts';

const FAMILIES = [
  'entry', 'keyed read', 'search', 'introspection', 'check', 'unit', 'numbering', 'row write', 'admin',
  'filter', 'walk', 'shape', 'projection', 'ids', 'iteration',
  'field write', 'list write', 'link', 'role', 'lifecycle', 'builder end',
  'load', 'unchecked write', 'raw read',
] as const;
type Family = (typeof FAMILIES)[number];

/** Each face's members, by family */
const FACES: Record<'engine' | 'admin' | 'query' | 'builder' | 'write', Record<string, Family>> = {
  engine: {
    query: 'entry',
    getRow: 'keyed read', getAttr: 'keyed read', getRoles: 'keyed read',
    findRelations: 'search',
    getSchemaStats: 'introspection',
    isEntityType: 'check', isRelationKind: 'check', isRole: 'check', wouldCreateCycle: 'check',
    transact: 'unit',
    nextLabel: 'numbering',
    create: 'row write', upsert: 'row write', write: 'row write',
    updateRelation: 'link',
    admin: 'admin',
  },
  admin: {
    clear: 'load', bulkLoadAttr: 'load', addToIndex: 'load',
    putAttr: 'unchecked write', deleteAttr: 'unchecked write', addRelation: 'unchecked write', updateRelation: 'unchecked write',
    getAttr: 'raw read', getAllEntities: 'raw read', getEntitiesOfType: 'raw read',
  },
  query: { ids: 'entry', all: 'entry', withTrashed: 'entry', trashed: 'entry' },
  builder: {
    where: 'filter', has: 'filter', lacks: 'filter', withRole: 'filter', ofType: 'filter', within: 'filter',
    linkedTo: 'filter', linkedFrom: 'filter', unlinkedTo: 'filter', unlinkedFrom: 'filter', linkedWith: 'filter',
    out: 'walk', in: 'walk', ancestors: 'walk', descendants: 'walk', outEdges: 'walk', inEdges: 'walk',
    orderBy: 'shape', reverse: 'shape', limit: 'shape', distinct: 'shape', groupBy: 'shape',
    pick: 'projection', pickOne: 'projection', pickAll: 'projection',
    ids: 'ids', first: 'ids', last: 'ids', count: 'ids', exists: 'ids',
    map: 'iteration', forEach: 'iteration', reduce: 'iteration',
  },
  write: {
    set: 'field write', setMany: 'field write', merge: 'field write', delete: 'field write',
    append: 'list write', removeAt: 'list write', removeValue: 'list write',
    link: 'link', unlink: 'link', unlinkAll: 'link',
    grant: 'role', revoke: 'role', grantExclusive: 'role',
    trash: 'lifecycle', restore: 'lifecycle', destroy: 'lifecycle', unload: 'lifecycle',
    id: 'builder end', row: 'builder end',
  },
};

/** What a face's members and its table disagree on: members with no family, and families' members that are gone */
function disagreement(members: readonly string[], table: Record<string, Family>) {
  return {
    unplaced: members.filter((member) => !Object.hasOwn(table, member)).sort(),
    gone: Object.keys(table).filter((member) => !members.includes(member)).sort(),
  };
}

function faces(): Record<keyof typeof FACES, string[]> {
  const engine = freshEngine({ isEntityType: (name) => name === 'Note', isRelationKind: () => true, isRole: () => true });
  const id = engine.create('Note').id();
  return {
    engine: Object.keys(engine),
    admin: Object.keys(engine.admin),
    query: Object.keys(engine.query),
    builder: Object.keys(engine.query.all()),
    write: Object.keys(engine.write(id)),
  };
}

describe('the engine vocabulary', () => {
  it.each(Object.keys(FACES) as (keyof typeof FACES)[])('places every %s member in one family', (face) => {
    const members = faces()[face];
    expect(members.length, `${face} has members to place`).toBeGreaterThan(0);
    expect(disagreement(members, FACES[face])).toEqual({ unplaced: [], gone: [] });
  });

  it('names only declared families', () => {
    const used = new Set(Object.values(FACES).flatMap((table) => Object.values(table)));
    expect(FAMILIES.filter((family) => !used.has(family)), 'families no member belongs to').toEqual([]);
  });

  it('reports a member with no family and a family member that is gone', () => {
    expect(disagreement(['where', 'whereNot'], { where: 'filter', has: 'filter' })).toEqual({ unplaced: ['whereNot'], gone: ['has'] });
  });
});
