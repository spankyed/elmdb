// An engine is an instance its holder acts on: two share nothing, and a typed facade acts on whichever engine its
// getter returns when called. Nothing in this package acts on an engine it wasn't given.
import { describe, expect, it } from 'vitest';
import { createEarsEngine, defineEars, type EARS, type Ears, type PersistenceSink } from '../src/index.ts';
import { recordingSink } from './engine/helpers.ts';

const types = new Set(['Note']);
const newEngine = (persistence?: PersistenceSink) => createEarsEngine({ isEntityType: (name) => types.has(name), isRelationKind: () => true, isRole: () => true, persistence });

describe('defineEars', () => {
  it('acts on the engine its getter returns at each call', () => {
    const first = newEngine();
    const second = newEngine();
    let current = first;
    const typed = defineEars<{ Note: { title: string } }, 'Note'>(() => current);

    const id = typed.create('Note', { title: 'first' }).id();
    expect(first.query.ids(id).pickOne()).toMatchObject({ title: 'first' });
    current = second;
    expect(typed.query('Note').ids()).toEqual([]);
    expect(typed.query.all().ids()).toEqual([]);
    current = first;
    expect(typed.query('Note').pickAll().map((row) => row.title)).toEqual(['first']);
    expect(typed.query.ids(id).count()).toBe(1);
  });

  it('builds without an engine, so a facade can be defined before one exists', () => {
    const typed = defineEars(() => { throw new Error('no engine yet'); });
    expect(typed.newId('Note')).toMatch(/^Note-/);
    expect(() => typed.query('Note')).toThrow('no engine yet');
  });
});

describe('two engines in one process', () => {
  it('share no data, relation index, repositories or persistence', () => {
    const a = recordingSink();
    const b = recordingSink();
    const one = newEngine(a.sink);
    const two = newEngine(b.sink);

    const x = one.create('Note').set('title', 'one').id();
    const y = one.create('Note').id();
    one.write(x).link('refs', y);
    two.create('Note').set('title', 'two');
    two.admin.bulkLoadAttr('Note-loaded' as EARS.EntityId, 'title', 'loaded');

    expect(one.query('Note').pick(['title']).map((row) => row.title)).toEqual(['one', undefined]);
    expect(two.query('Note').pick(['title']).map((row) => row.title)).toEqual(['two', 'loaded']);
    expect(one.findRelations()).toHaveLength(1);
    expect(two.findRelations()).toEqual([]);
    expect(Object.keys(two.getSchemaStats().relations)).toEqual([]);
    expect(b.calls.every(([, , id]) => id !== x)).toBe(true);
    expect(a.calls.filter(([method]) => method === 'onAddRelation')).toHaveLength(1);
    expect(b.calls.filter(([method]) => method === 'onAddRelation')).toHaveLength(0);

    one.admin.clear();
    expect(one.admin.getAllEntities()).toEqual([]);
    expect(two.admin.getAllEntities()).toHaveLength(2);
  });
});

describe('the engine', () => {
  it('is its operations, with the creator\'s privileges beside them in admin', () => {
    const engine = newEngine();
    const id = engine.create('Note').set('title', 'held').id();
    expect(engine.query.ids(id).pickOne()).toMatchObject({ title: 'held' });
    engine.admin.clear();
    expect(engine.admin.getAllEntities()).toEqual([]);

    // A program is handed the engine as `Ears`, which has no `admin`. This line stops compiling the moment `Ears`
    // gains an `admin` member, which would hand every program the creator's privileges.
    const handed: Ears = engine;
    // @ts-expect-error `Ears` has no `admin`
    void handed.admin;
  });
});
