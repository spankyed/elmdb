// The free functions act on the installed engine; with none installed they throw, naming the fix. Engines share nothing.
import { afterEach, describe, expect, it } from 'vitest';
import {
  createEarsEngine, defineEars, findRelations, getAllEntities, installEngine, installedEngine, isEntityType,
  registerRepository, unregisterRepository, repository, spawn, untypedCreate, untypedUpsert, untypedWrite, untypedQx, transact, bp, type EARS, type PersistenceSink,
} from '../src/index.ts';
import { recordingSink } from './engine/helpers.ts';

const types = new Set(['Note']);
const newEngine = (persistence?: PersistenceSink) => createEarsEngine({ isEntityType: (name) => types.has(name), persistence });

afterEach(() => { installEngine(undefined); });

describe('with no engine installed', () => {
  it('the free functions throw, naming how to install one', () => {
    const typed = defineEars<{ Note: { title: string } }, 'Note'>();
    const uses: Array<[string, () => unknown]> = [
      ['qx', () => untypedQx('Note')],
      ['qx.ids', () => untypedQx.ids('Note-1' as EARS.EntityId)],
      ['qx.all', () => untypedQx.all()],
      ['untypedCreate', () => untypedCreate('Note')],
      ['untypedUpsert', () => untypedUpsert('Note', 'Note-1' as EARS.EntityId)],
      ['untypedWrite', () => untypedWrite('Note-1' as EARS.EntityId)],
      ['transact', () => transact(() => 1)],
      ['typed qx', () => typed.qx('Note')],
      ['typed qx.all', () => typed.qx.all()],
      ['typed create', () => typed.create('Note')],
      ['typed write', () => typed.write('Note-1' as EARS.EntityId<'Note'>)],
      ['typed findAll', () => typed.findAll('Note')],
      ['repository', () => repository.noteQueries],
      ['registerRepository', () => registerRepository('noteQueries', {})],
      ['unregisterRepository', () => unregisterRepository('noteQueries')],
      ['findRelations', () => findRelations()],
      ['getAllEntities', () => getAllEntities()],
      ['isEntityType', () => isEntityType('Note')],
      ['spawn', () => spawn(bp('Note').build())],
      ['installedEngine', () => installedEngine()],
    ];
    for (const [name, use] of uses) {
      expect(use, name).toThrow('No EARS engine is installed');
    }
  });

  it('pure helpers still work', () => {
    expect(bp('Note').attr('title', 'x').build()).toEqual({ entity: 'Note', attrs: { title: 'x' } });
    expect(defineEars().createEntity('Note')).toMatch(/^Note-/);
  });
});

describe('installEngine', () => {
  it('points the free functions at an engine and returns the one it replaced', () => {
    const first = newEngine();
    const second = newEngine();
    expect(installEngine(first.query)).toBeUndefined();
    const id = untypedCreate('Note').set('title', 'first').id();
    expect(installEngine(second.query)).toBe(first.query);
    expect(untypedQx('Note').ids()).toEqual([]);
    expect(installedEngine()).toBe(second.query);
    installEngine(first.query);
    expect(defineEars<{ Note: { title: string } }, 'Note'>().findAll('Note').map((row) => row.title)).toEqual(['first']);
    expect(first.query.findById(id)).toMatchObject({ title: 'first' });
  });
});

describe('two engines in one process', () => {
  it('share no data, relation index, repositories or persistence', () => {
    const a = recordingSink();
    const b = recordingSink();
    const one = newEngine(a.sink);
    const two = newEngine(b.sink);

    const x = one.query.create('Note').set('title', 'one').id();
    const y = one.query.create('Note').id();
    one.query.write(x).link('refs', y);
    one.query.registerRepository('noteQueries', { name: 'one' });
    two.query.create('Note').set('title', 'two');
    two.admin.bulkLoadAttr('Note-loaded' as EARS.EntityId, 'title', 'loaded');

    expect(one.query.qx('Note').pick(['title']).map((row) => row.title)).toEqual(['one', null]);
    expect(two.query.qx('Note').pick(['title']).map((row) => row.title)).toEqual(['two', 'loaded']);
    expect(one.query.findRelations()).toHaveLength(1);
    expect(two.query.findRelations()).toEqual([]);
    expect(two.admin.relationIndex).toEqual({});
    expect(one.query.repository.noteQueries).toEqual({ name: 'one' });
    expect(() => two.query.repository.noteQueries).toThrow('"noteQueries" is not registered');
    expect(b.calls.every(([, , id]) => id !== x)).toBe(true);
    expect(a.calls.filter(([method]) => method === 'onAddRelation')).toHaveLength(1);
    expect(b.calls.filter(([method]) => method === 'onAddRelation')).toHaveLength(0);

    one.admin.clear();
    expect(one.query.getAllEntities()).toEqual([]);
    expect(two.query.getAllEntities()).toHaveLength(2);
    expect(one.query.repository.noteQueries).toEqual({ name: 'one' });
    expect(one.admin.repositories()).toEqual({ noteQueries: { name: 'one' } });
  });
});

describe('the repository registry', () => {
  it('reads a registered repository until it is unregistered', () => {
    const engine = newEngine();
    installEngine(engine.query);
    const noteQueries = { name: 'notes' };
    registerRepository('noteQueries', noteQueries);
    expect(repository.noteQueries).toBe(noteQueries);

    unregisterRepository('noteQueries');

    expect(() => repository.noteQueries).toThrow('"noteQueries" is not registered');
    expect(engine.admin.repositories()).toEqual({});
  });
});
