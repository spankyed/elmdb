// watch: a read runs again after each commit that wrote into what it read, and its listener hears each result that
// changed, after the writing code has returned, never inside a write. The guarantee that matters most — a watch never
// misses a change, whatever writes it — is the model-based harness's (model-based.spec.ts); these cases are the
// contract's edges, each with a reading of how many times a read ran where that is the point.
import { execFile } from 'node:child_process';
import { createRequire } from 'node:module';
import { promisify } from 'node:util';
import { beforeEach, describe, expect, it } from 'vitest';
import { engine, NAMES, type Id } from './helpers.ts';
import { freshEngine } from './engine-under-test.ts';
import type { EngineUnderTest } from './engine-under-test.ts';
import { createConnection, runCommitted, type Commit, type EarsDatabase } from '../../src/database.ts';

let e: EngineUnderTest;
beforeEach(() => { e = engine(); });

/** A watch that keeps what it was handed, and counts how many times its read ran */
function watched<T>(read: () => T) {
  const seen: Array<{ value: T; commit: number }> = [];
  const errors: unknown[] = [];
  let runs = 0;
  const watching = e.watch(() => { runs++; return read(); }, {
    next: (value, commit) => { seen.push({ value, commit }); },
    error: (error) => { errors.push(error); },
  });
  return { seen, errors, runs: () => runs, last: () => seen.at(-1)?.value, stop: () => watching.stop() };
}

const settled = () => e.admin.watchesSettled();
/** Every task's title, in creation order */
const titles = () => e.query('Task').ids().map((id) => e.getAttr(id, 'title'));

/** Commits to the database from a process of its own, as a tool killed before telling anyone leaves it */
const commitElsewhere = async (database: EarsDatabase, create?: string) => {
  const requireFromEars = createRequire(createRequire(import.meta.url).resolve('@apackdev/ears/package.json'));
  const elmdb = requireFromEars.resolve('elmdb');
  const msgpackr = requireFromEars.resolve('msgpackr');
  const [, type, id] = create?.match(/create\('(\w+)', '([\w-]+)'\)/) ?? [];
  await promisify(execFile)(process.execPath, ['-e', [
    `const { Database } = require(${JSON.stringify(elmdb)});`,
    `const { Packr } = require(${JSON.stringify(msgpackr)});`,
    'const database = Database.open(process.argv[1]);',
    type
      ? `database.write(new Packr({ useRecords: false }).pack({ ops: [{ op: 'create', type: ${JSON.stringify(type)}, id: ${JSON.stringify(id)}, fields: {} }], now: 1 }));`
      : 'database.begin().commit(false);',
    'database.close();',
  ].join('\n'), database.path]);
};

it('runs again when told another process committed, which it cannot hear for itself', async () => {
  let database: EarsDatabase | undefined;
  const told = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
  const seen: string[][] = [];
  const watching = told.watch(() => told.query('Task').ids().map(String), { next: (value) => { seen.push(value); } });
  await told.admin.watchesSettled();
  expect(seen).toEqual([[]]);

  // Nothing here dispatches it
  await commitElsewhere(database!, "create('Task', 'Task-elsewhere')");
  await told.admin.watchesSettled();
  expect(seen, 'nobody has said so yet').toEqual([[]]);

  told.admin.changedElsewhere();
  await told.admin.watchesSettled();
  expect(seen).toEqual([[], ['Task-elsewhere']]);
  watching.stop();
});

describe('a commit from another process', () => {
  it('wakes a listener with nothing committed here, and the watches run when it asks', async () => {
    let database: EarsDatabase | undefined;
    const found = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
    let runs = 0;
    let told = () => {};
    const woken = new Promise<void>((resolve) => { told = resolve; });
    found.admin.onFoundElsewhere(() => told());
    const watching = found.watch(() => { runs++; return found.query('Project').count(); }, { next: () => {} });
    await found.admin.watchesSettled();
    expect(runs).toBe(1);

    await commitElsewhere(database!);
    await woken;
    expect(found.admin.changedElsewhere()).toBe(true);
    await found.admin.watchesSettled();
    expect(runs).toBe(2);
    watching.stop();
  });

  it('is found at the next commit here when the wake never came, and said once', async () => {
    let database: EarsDatabase | undefined;
    const found = freshEngine({ ...NAMES, wrap: (held) => (database = new Proxy(held, {
      get(target, key) {
        if (key === 'hearCommitsElsewhere') return () => {};
        const value = Reflect.get(target, key);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    })) });
    let told = 0;
    found.admin.onFoundElsewhere(() => { told++; });
    await commitElsewhere(database!);
    await Promise.resolve();
    expect(told, 'nothing here has committed').toBe(0);

    found.create('Task', { title: 'mine' });
    await Promise.resolve();
    expect(told).toBe(1);
    expect(found.admin.changedElsewhere()).toBe(true);
    found.create('Task', { title: 'mine too' });
    await Promise.resolve();
    expect(told, 'said once').toBe(1);
  });

  it('keeps the listener for the next commit here when the database cannot hear', async () => {
    let database: EarsDatabase | undefined;
    const found = freshEngine({ ...NAMES, wrap: (held) => (database = new Proxy(held, {
      get(target, key) {
        if (key === 'hearCommitsElsewhere') return () => { throw new Error('no loopback here'); };
        const value = Reflect.get(target, key);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    })) });
    let told = 0;
    expect(() => found.admin.onFoundElsewhere(() => { told++; })).toThrow('no loopback here');
    await commitElsewhere(database!);
    found.create('Task', { title: 'mine' });
    await Promise.resolve();
    expect(told).toBe(1);
  });

  it('reads whether another process committed, skipping its own, and runs nothing if none did', async () => {
    let database: EarsDatabase | undefined;
    const reader = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
    let runs = 0;
    const watching = reader.watch(() => { runs++; return reader.query('Project').count(); }, { next: () => {} });
    await reader.admin.watchesSettled();

    reader.create('Task', { title: 'mine' });
    await reader.admin.watchesSettled();
    expect(reader.admin.changedElsewhere(), 'its own commits are not news').toBe(false);
    await reader.admin.watchesSettled();
    expect(runs, 'and nothing ran').toBe(1);

    await commitElsewhere(database!, "create('Task', 'Task-elsewhere')");
    expect(reader.admin.changedElsewhere()).toBe(true);
    await reader.admin.watchesSettled();
    expect(runs).toBe(2);
    expect(reader.admin.changedElsewhere(), 'read once').toBe(false);
    watching.stop();
  });

});

describe('another process replacing every row', () => {
  const resetElsewhere = async (database: EarsDatabase) => {
    const elmdb = createRequire(createRequire(import.meta.url).resolve('@apackdev/ears/package.json')).resolve('elmdb');
    await promisify(execFile)(process.execPath, ['-e', [
      `const { Database } = require(${JSON.stringify(elmdb)});`,
      'const database = Database.open(process.argv[1]);',
      'database.reset();',
      'database.close();',
    ].join('\n'), database.path]);
  };

  it('refuses writes until adopted, saying so even without a wake, and runs every watch when it is', async () => {
    let database: EarsDatabase | undefined;
    const held = freshEngine({ ...NAMES, wrap: (given) => (database = new Proxy(given, {
      get(target, key) {
        if (key === 'hearCommitsElsewhere') return () => {};
        const value = Reflect.get(target, key);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    })) });
    let told = 0;
    held.admin.onFoundElsewhere(() => { told++; });
    held.create('Task', { title: 'before' });
    let runs = 0;
    const watching = held.watch(() => { runs++; return held.query('Task').count(); }, { next: () => {} });
    await held.admin.watchesSettled();
    expect(runs).toBe(1);

    await resetElsewhere(database!);
    expect(held.admin.replacedElsewhere()).toBe(true);
    expect(held.query('Task').count(), 'reads read what is there now').toBe(0);
    expect(() => held.create('Task', { title: 'plain' })).toThrow(expect.objectContaining({ code: 'DATA_REPLACED' }));
    expect(() => held.transact(() => { held.create('Task', { title: 'in a unit' }); })).toThrow(expect.objectContaining({ code: 'DATA_REPLACED' }));
    await Promise.resolve();
    expect(told).toBe(2);

    held.admin.adoptReplacement();
    await held.admin.watchesSettled();
    expect(runs).toBe(2);
    expect(held.admin.replacedElsewhere()).toBe(false);
    expect(held.admin.changedElsewhere(), 'adopting moved past it').toBe(false);
    held.create('Task', { title: 'after' });
    expect(held.query('Task').count()).toBe(1);
    watching.stop();
  });
});

describe('watch', () => {
  it('delivers the first result after the call returns, not during it', async () => {
    e.create('Task', { title: 'one' });
    const w = watched(() => titles());
    expect(w.seen, 'nothing is delivered inside watch()').toHaveLength(0);
    await settled();
    expect(w.last()).toEqual(['one']);
  });

  it('runs again after a commit that touches what it read, and delivers the new result with that commit', async () => {
    const w = watched(() => titles());
    await settled();
    const task = e.create('Task', { title: 'new' }).id();
    expect(w.seen, 'nothing is delivered inside the write').toHaveLength(1);
    await settled();
    expect(w.last()).toEqual(['new']);
    expect(w.seen.at(-1)!.commit).toBe(e.lastCommit());

    e.write(task).set('title', 'renamed');
    await settled();
    expect(w.last()).toEqual(['renamed']);
    e.write(task).destroy();
    await settled();
    expect(w.last()).toEqual([]);
  });

  it('leaves a read of one entity alone when another entity changes', async () => {
    const a = e.create('Task', { title: 'a' }).id();
    const b = e.create('Task', { title: 'b' }).id();
    const w = watched(() => e.getRow(a)?.title);
    await settled();
    expect(w.runs()).toBe(1);
    e.write(b).set('title', 'b, changed');
    e.write(b).link('refs', a);
    await settled();
    expect(w.runs(), "b's writes are not in a's read").toBe(1);
    e.write(a).set('title', 'a, changed');
    await settled();
    expect(w.runs()).toBe(2);
    expect(w.last()).toBe('a, changed');
  });

  it('runs once for a burst of commits made before the writing code returns', async () => {
    const w = watched(() => e.query('Task').count());
    await settled();
    for (let i = 0; i < 50; i++) e.create('Task', { title: `t${i}` });
    await settled();
    expect(w.runs()).toBe(2);
    expect(w.seen.map((s) => s.value)).toEqual([0, 50]);
  });

  it('delivers nothing when the result it runs to is the one last delivered', async () => {
    const task = e.create('Task', { title: 'same', done: false }).id();
    const w = watched(() => titles());
    await settled();
    e.write(task).set('done', true);
    await settled();
    expect(w.runs(), 'no title was written, so nothing ran it').toBe(1);
    e.write(task).set('title', 'same');
    await settled();
    expect(w.runs(), 'a title was written, so it ran').toBe(2);
    expect(w.seen, 'to the titles it had').toHaveLength(1);
  });

  it('delivers a map or a set whose contents changed, and not one rebuilt with the same contents', async () => {
    const task = e.create('Task', { title: 'a' }).id();
    const byTitle = watched(() => new Map(titles().map((title) => [title, String(title).length])));
    const titleSet = watched(() => new Set(titles()));
    await settled();
    e.write(task).set('title', 'a');
    await settled();
    expect(byTitle.seen, 'the same entries').toHaveLength(1);
    expect(titleSet.seen, 'the same members').toHaveLength(1);
    e.write(task).set('title', 'b');
    await settled();
    expect(byTitle.last()).toEqual(new Map([['b', 1]]));
    expect(titleSet.last()).toEqual(new Set(['b']));
  });

  it('hears a durable unit that committed and then failed to sync', async () => {
    // A database whose units commit and then report the sync failing, as elmdb's NOT_DURABLE does. A native method
    // needs its own object as `this`
    const bound = (target: object, key: PropertyKey) => {
      const value: unknown = Reflect.get(target, key);
      return typeof value === 'function' ? value.bind(target) : value;
    };
    const failingSync = (database: EarsDatabase): EarsDatabase => new Proxy(database, {
      get(target, key) {
        if (key !== 'begin') return bound(target, key);
        return () => {
          const unit = target.begin();
          return new Proxy(unit, {
            get(held, member) {
              if (member !== 'commit') return bound(held, member);
              return () => {
                held.commit(false);
                throw Object.assign(new Error('the sync failed'), { code: 'NOT_DURABLE' });
              };
            },
          });
        };
      },
    });
    e = engine({ wrap: failingSync });
    const w = watched(() => titles());
    await settled();
    expect(() => e.transact(() => { e.create('Task', { title: 'kept' }); }, { durable: true })).toThrow(/sync failed/);
    await settled();
    expect(w.last(), 'the commit stands, and the watch heard it').toEqual(['kept']);
  });

  it('hears nothing of a unit that threw, and the unit inside one that rolled back', async () => {
    const w = watched(() => e.query('Task').count());
    await settled();
    expect(() => e.transact(() => { e.create('Task', { title: 'gone' }); throw new Error('no'); })).toThrow('no');
    await settled();
    expect(w.runs(), 'a unit that threw committed nothing').toBe(1);

    e.transact(() => {
      e.create('Task', { title: 'kept' });
      expect(() => e.transact(() => { e.create('Task', { title: 'undone' }); throw new Error('inner'); })).toThrow('inner');
    });
    await settled();
    expect(w.last()).toBe(1);
  });

  it('keeps a read that threw watched, and delivers its result once it answers', async () => {
    const w = watched(() => {
      const [task] = e.query('Task').ids();
      if (!task) throw new Error('no task yet');
      return e.getRow(task)?.title;
    });
    await settled();
    expect(w.errors).toHaveLength(1);
    e.create('Task', { title: 'arrived' });
    await settled();
    expect(w.last()).toBe('arrived');
  });

  it('runs every watch again when the store is emptied', async () => {
    e.create('Task', { title: 'x' });
    const w = watched(() => e.query('Task').count());
    await settled();
    e.admin.clear();
    await settled();
    expect(w.last()).toBe(0);
  });

  it('delivers nothing once stopped, and leaves nothing filed', async () => {
    const w = watched(() => titles());
    await settled();
    expect(e.admin.watchStats().watches).toBe(1);
    w.stop();
    e.create('Task', { title: 'after' });
    await settled();
    expect(w.seen).toHaveLength(1);
    expect(e.admin.watchStats()).toEqual({ watches: 0, keys: 0, ranges: 0 });
  });

  it('runs again when a commit enters a range a limited read stopped short of only if it lands before where it stopped', async () => {
    const ids: Id[] = [];
    for (let i = 0; i < 5; i++) ids.push(e.create('Task', { title: `t${i}` }).id());
    const w = watched(() => e.query('Task').first());
    await settled();
    e.write(ids[4]!).set('title', 'changed, past the first');
    await settled();
    expect(w.runs(), 'the read stopped at the first task, so a later one is not in its read').toBe(1);
    e.write(ids[0]!).destroy();
    await settled();
    expect(w.last()).toBe(ids[1]);
  });

  it('is a pure read of the engine: what it reads anywhere else is not tracked', async () => {
    let outside = 'before';
    const w = watched(() => `${e.query('Task').count()} ${outside}`);
    await settled();
    outside = 'after';
    await settled();
    expect(w.last(), 'nothing in the store changed, so nothing ran it again').toBe('0 before');
    e.create('Task', { title: 'x' });
    await settled();
    expect(w.last()).toBe('1 after');
  });

  it('numbers commits in increasing order, one per unit that wrote', () => {
    const before = e.lastCommit();
    e.create('Task', { title: 'one' });
    e.transact(() => { e.create('Task', { title: 'two' }); e.create('Task', { title: 'three' }); });
    e.query('Task').ids();
    expect(e.lastCommit()).toBe(before + 2);
  });
});

/** Native methods require their own receiver; the wrapper changes only the outcome after a real commit. */
function committedFailure(method: keyof EarsDatabase, before = false, refusalCode: 'IO' | 'SCHEMA_CHANGED' = 'IO') {
  return (database: EarsDatabase): EarsDatabase => new Proxy(database, {
    get(target, key) {
      const value: unknown = Reflect.get(target, key, target);
      if (key !== method) return typeof value === 'function' ? value.bind(target) : value;
      return (...args: unknown[]) => {
        if (before) throw Object.assign(new Error('refused before commit'), { code: refusalCode });
        (value as (...args: unknown[]) => unknown).apply(target, args);
        throw Object.assign(new Error('the sync failed after commit'), { code: 'NOT_DURABLE' });
      };
    },
  });
}

describe('admin committed errors', () => {
  it.each(['reset', 'bulkLoad'] as const)('notifies a committed %s before rethrowing', async (method) => {
    e = engine({ wrap: committedFailure(method) });
    const id = e.create('Task', { title: 'before' }).id();
    const w = watched(titles);
    await settled();
    const operation = () => method === 'reset' ? e.admin.clear() : e.admin.putAttr(id, 'title', 'after');
    expect(operation).toThrow('the sync failed after commit');
    await settled();
    expect(w.last()).toEqual(method === 'reset' ? [] : ['after']);
    expect(w.runs()).toBe(2);
    expect(w.seen.at(-1)!.commit).toBe(e.lastCommit());
  });

  it.each(['setSchema', 'declare'] as const)('refreshes declarations and notifies a committed %s', async (method) => {
    e = engine({ wrap: committedFailure('setSchema') });
    e.create('Task', { title: 'before' });
    const w = watched(() => {
      const count = e.query('Task').count();
      try { e.query('Novel').count(); return [count, true]; }
      catch { return [count, false]; }
    });
    await settled();
    expect(w.last()).toEqual([1, false]);
    expect(() => e.admin[method]({ types: { Task: { required: [] }, Novel: { required: [] } }, relations: {}, roles: [] }))
      .toThrow('the sync failed after commit');
    await settled();
    expect(w.last()).toEqual([1, true]);
    expect(w.runs()).toBe(2);
    expect(() => e.create('Novel', {})).not.toThrow();
  });

  it.each(['reset', 'bulkLoad', 'setSchema'] as const)('does not notify an uncommitted %s refusal', async (method) => {
    e = engine({ wrap: committedFailure(method, true) });
    const id = e.create('Task', { title: 'before' }).id();
    const w = watched(titles);
    await settled();
    const sequence = e.lastCommit();
    expect(() => {
      if (method === 'reset') e.admin.clear();
      else if (method === 'bulkLoad') e.admin.putAttr(id, 'title', 'after');
      else e.admin.declare({ types: { Novel: { required: [] } }, relations: {}, roles: [] });
    }).toThrow('refused before commit');
    await settled();
    expect(w.last()).toEqual(['before']);
    expect(w.runs()).toBe(1);
    expect(e.lastCommit()).toBe(sequence);
    expect(() => e.query('Novel').count()).toThrow();
  });

  it('does not notify or adopt declarations after a SCHEMA_CHANGED refusal', async () => {
    e = engine({ wrap: committedFailure('setSchema', true, 'SCHEMA_CHANGED') });
    e.create('Task', { title: 'before' });
    const w = watched(titles);
    await settled();
    const sequence = e.lastCommit();
    expect(() => e.admin.declare({ types: { Novel: { required: [] } }, relations: {}, roles: [] }))
      .toThrow(expect.objectContaining({ code: 'SCHEMA_CHANGED' }));
    await settled();
    expect(w.last()).toEqual(['before']);
    expect(w.runs()).toBe(1);
    expect(e.lastCommit()).toBe(sequence);
    expect(() => e.query('Novel').count()).toThrow();
    w.stop();
  });

  it('notifies a replacement that commits and then reports failed sync', async () => {
    const { mkdtempSync, rmSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('node:path');
    const dir = mkdtempSync(join(tmpdir(), 'ears-committed-replace-'));
    try {
      let backing: EarsDatabase | undefined;
      e = engine({ wrap: (db) => { backing = db; return committedFailure('replaceWith')(db); } });
      const id = e.create('Task', { title: 'backup' }).id();
      backing!.backup(join(dir, 'backup'));
      e.write(id).set('title', 'before');
      const w = watched(titles);
      await settled();
      expect(() => e.admin.replaceWith(join(dir, 'backup'))).toThrow('the sync failed after commit');
      await settled();
      expect(w.last()).toEqual(['backup']);
      expect(w.runs()).toBe(2);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});


it('reports a committed load to connection listeners exactly once', () => {
  let database: EarsDatabase | undefined;
  e = engine({ wrap: (raw) => { database = committedFailure('bulkLoad')(raw); return database; } });
  const connection = createConnection(database!);
  const heard: Commit[] = [];
  connection.onCommit((commit) => heard.push(commit));
  expect(() => connection.load([{ load: 'entity', id: 'Task-committed', fields: { title: 'kept' } }]))
    .toThrow('the sync failed after commit');
  expect(heard).toHaveLength(1);
  expect(heard[0]).toEqual({ sequence: connection.lastCommit(), touched: 'all' });
  expect(e.query('Task').count()).toBe(1);
});


it('preserves failed durability when refreshing committed state also throws', () => {
  const durability = Object.assign(new Error('sync failed'), { code: 'NOT_DURABLE' });
  const refresh = new Error('schema refresh failed');
  let failure: unknown;
  try { runCommitted(() => { throw durability; }, () => { throw refresh; }); }
  catch (error) { failure = error; }
  expect(failure).toBeInstanceOf(AggregateError);
  expect((failure as AggregateError & { code: string }).code).toBe('NOT_DURABLE');
  expect((failure as AggregateError).errors).toEqual([durability, refresh]);
  expect((failure as Error).cause).toBe(durability);
});
