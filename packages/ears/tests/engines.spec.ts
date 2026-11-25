// An engine is an instance its holder acts on: two share nothing, and a typed facade acts on whichever engine its
// getter returns when called. Nothing in this package acts on an engine it wasn't given.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { describe, expect, it } from 'vitest';
import { defineEars, type EARS, type Ears, type UntypedSchema } from '../src/index.ts';
import { createTemporaryDatabases } from '../src/lmdb/index.ts';
import { freshEngine } from './engine/engine-under-test.ts';

const newEngine = () => freshEngine({ types: ['Note'], relationKinds: ['refs'], roles: [] });

describe('defineEars', () => {
  it('acts on the engine its getter returns at each call', () => {
    const first = newEngine();
    const second = newEngine();
    let current = first;
    const typed = defineEars<UntypedSchema & { shapes: { Note: { title: string } }; names: 'Note' }>(() => current);

    const id = typed.create('Note', { title: 'first' }).id();
    expect(first.query.ids(id).pickOne()).toMatchObject({ title: 'first' });
    current = second;
    expect(typed.query('Note').ids()).toEqual([]);
    expect(typed.query.all().ids()).toEqual([]);
    current = first;
    expect(typed.query('Note').pickAll().map((row) => row.title)).toEqual(['first']);
    expect(typed.newId('Note')).toMatch(/^Note-/);
    expect(typeof typed.now()).toBe('number');
    expect(typed.query.ids(id).count()).toBe(1);
  });

  it('builds without an engine, so a facade can be defined before one exists', () => {
    const typed = defineEars(() => { throw new Error('no engine yet'); });
    expect(() => typed.query('Note')).toThrow('no engine yet');
    expect(() => typed.newId('Note')).toThrow('no engine yet');
    expect(() => typed.now()).toThrow('no engine yet');
  });
});

describe('two engines in one process', () => {
  it('share no data, relation index, repositories or store', () => {
    const one = newEngine();
    const two = newEngine();

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
    expect(two.getRow(x)).toBeUndefined();

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

describe('createTemporaryDatabases', () => {
  const validSchema = { types: { Note: { required: [] } }, relations: {}, roles: [] };

  it('cleans up the temp directory and leaks no exit listener when open() fails', () => {
    const beforeListeners = process.listenerCount('exit');
    const isolated = fs.mkdtempSync(path.join(os.tmpdir(), 'ears-isolated-'));
    const saved = { TMPDIR: process.env.TMPDIR, TMP: process.env.TMP, TEMP: process.env.TEMP };
    process.env.TMPDIR = isolated;
    process.env.TMP = isolated;
    process.env.TEMP = isolated;
    try {
      const temps = createTemporaryDatabases();
      expect(process.listenerCount('exit')).toBe(beforeListeners);

      expect(() => temps.open({ types: { ['__proto__']: { required: [] } }, relations: {}, roles: [] })).toThrow();
      expect(process.listenerCount('exit')).toBe(beforeListeners);
      expect(fs.readdirSync(isolated)).toEqual([]);
    } finally {
      for (const [key, value] of Object.entries(saved)) {
        if (value === undefined) delete process.env[key];
        else process.env[key] = value;
      }
      fs.rmSync(isolated, { recursive: true, force: true });
    }
  });

  it('detaches its exit listener once all opened databases are disposed and cleans remaining handles on partial failure', () => {
    const beforeListeners = process.listenerCount('exit');
    const temps = createTemporaryDatabases();
    const first = temps.open(validSchema);
    const second = temps.open(validSchema);
    const firstPath = first.database.path;
    const secondPath = second.database.path;
    expect(process.listenerCount('exit')).toBe(beforeListeners + 1);

    // Hold a unit open on the first database so close() fails with UNIT_OPEN during disposeAll()
    const unit = first.database.begin();
    try {
      expect(() => temps.disposeAll()).toThrow(expect.objectContaining({ code: 'UNIT_OPEN' }));
      expect(second.database.isOpen, 'second database was still closed').toBe(false);
      expect(fs.existsSync(secondPath), 'second temp dir was still removed').toBe(false);
      expect(process.listenerCount('exit'), 'exit listener detached after disposeAll').toBe(beforeListeners);
    } finally {
      if (unit.isOpen) unit.abort();
      if (first.database.isOpen) first.database.close();
      fs.rmSync(firstPath, { recursive: true, force: true });
    }

    // Reusing the manager attaches and detaches cleanly again
    const third = temps.open(validSchema);
    expect(process.listenerCount('exit')).toBe(beforeListeners + 1);
    third.dispose();
    third.dispose();
    expect(process.listenerCount('exit')).toBe(beforeListeners);
  });
});
