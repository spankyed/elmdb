// An engine is an instance its holder acts on: two share nothing, and a typed facade acts on whichever engine its
// getter returns when called. Nothing in this package acts on an engine it wasn't given.
import { describe, expect, it } from 'vitest';
import { createEarsEngine, defineEars, type EARS, type PersistenceSink } from '../src/index.ts';
import { recordingSink } from './engine/helpers.ts';

const types = new Set(['Note']);
const newEngine = (persistence?: PersistenceSink) => createEarsEngine({ isEntityType: (name) => types.has(name), persistence });

describe('defineEars', () => {
  it('acts on the engine its getter returns at each call', () => {
    const first = newEngine();
    const second = newEngine();
    let current = first;
    const typed = defineEars<{ Note: { title: string } }, 'Note'>(() => current.query);

    const id = typed.create('Note').set('title', 'first').id();
    expect(first.query.findById(id)).toMatchObject({ title: 'first' });
    current = second;
    expect(typed.qx('Note').ids()).toEqual([]);
    expect(typed.qx.all().ids()).toEqual([]);
    current = first;
    expect(typed.findAll('Note').map((row) => row.title)).toEqual(['first']);
    expect(typed.qx.ids(id).count()).toBe(1);
  });

  it('builds without an engine, so a facade can be defined before one exists', () => {
    const typed = defineEars(() => { throw new Error('no engine yet'); });
    expect(typed.createEntity('Note')).toMatch(/^Note-/);
    expect(() => typed.qx('Note')).toThrow('no engine yet');
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
    const noteQueries = { name: 'notes' };
    engine.query.registerRepository('noteQueries', noteQueries);
    expect(engine.query.repository.noteQueries).toBe(noteQueries);

    engine.query.unregisterRepository('noteQueries');

    expect(() => engine.query.repository.noteQueries).toThrow('"noteQueries" is not registered');
    expect(engine.admin.repositories()).toEqual({});
  });
});
