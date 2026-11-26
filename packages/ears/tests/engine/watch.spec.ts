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
import { sameValue } from '../../src/watch.ts';

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
  const requireFromEars = createRequire(createRequire(import.meta.url).resolve('@elmdb/ears/package.json'));
  const elmdb = requireFromEars.resolve('elmdb');
  const msgpackr = requireFromEars.resolve('msgpackr');
  const [, type, id] = create?.match(/create\('(\w+)', '([\w-]+)'\)/) ?? [];
  await promisify(execFile)(process.execPath, ['-e', [
    `const { Database } = require(${JSON.stringify(elmdb)});`,
    `const { Packr } = require(${JSON.stringify(msgpackr)});`,
    // In the engine's mode, syncs: false: elmdb refuses a writer of another mode beside a live one (MODE_MISMATCH)
    'const database = Database.open(process.argv[1], { syncs: false });',
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

  it('is found at the next commit here when the wake never came', async () => {
    let database: EarsDatabase | undefined;
    const found = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
    await commitElsewhere(database!);
    // Listening only now, so the other process's wake went to nobody
    const told = new Promise<void>((resolve) => { found.admin.onFoundElsewhere(resolve); });
    found.create('Task', { title: 'mine' });
    await told;
    expect(found.admin.changedElsewhere()).toBe(true);
  });

  it('is not found in a sequence this process skipped itself, as its index builder does', async () => {
    let database: EarsDatabase | undefined;
    const found = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
    let told = 0;
    found.admin.onFoundElsewhere(() => { told++; });
    // Another handle on the same database commits as this process does: under its origin, unheard here
    createConnection(database!).write([{ op: 'create', type: 'Task', id: 'Task-mine-too', fields: { title: 'here' } }]);
    found.create('Task', { title: 'mine' });
    await Promise.resolve();
    expect(told).toBe(0);
  });

  it('keeps the listener for the next commit here when the database cannot hear', async () => {
    let database: EarsDatabase | undefined;
    const found = freshEngine({ ...NAMES, wrap: (held) => (database = new Proxy(held, {
      get(target, key) {
        if (key === 'hearCommitsElsewhere') return () => { throw new Error('nothing may listen here'); };
        const value = Reflect.get(target, key);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    })) });
    let told = 0;
    expect(() => found.admin.onFoundElsewhere(() => { told++; })).toThrow('nothing may listen here');
    await commitElsewhere(database!);
    found.create('Task', { title: 'mine' });
    await Promise.resolve();
    expect(told).toBe(1);
    found.create('Task', { title: 'mine too' });
    await Promise.resolve();
    expect(told, 'said once').toBe(1);
    expect(found.admin.changedElsewhere(), 'and answered when asked').toBe(true);
    expect(found.admin.changedElsewhere()).toBe(false);
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
    const elmdb = createRequire(createRequire(import.meta.url).resolve('@elmdb/ears/package.json')).resolve('elmdb');
    await promisify(execFile)(process.execPath, ['-e', [
      `const { Database } = require(${JSON.stringify(elmdb)});`,
      // In the engine's mode, as commitElsewhere opens it
      'const database = Database.open(process.argv[1], { syncs: false });',
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
    const epoch = held.admin.replacedElsewhere();
    expect(epoch).toBeTypeOf('number');
    expect(held.query('Task').count(), 'reads read what is there now').toBe(0);
    expect(() => held.create('Task', { title: 'plain' })).toThrow(expect.objectContaining({ code: 'DATA_REPLACED' }));
    expect(() => held.transact(() => { held.create('Task', { title: 'in a unit' }); })).toThrow(expect.objectContaining({ code: 'DATA_REPLACED' }));
    await Promise.resolve();
    expect(told).toBe(2);

    expect(held.admin.adoptReplacement(epoch! + 1), 'not the replacement that landed').toBe(false);
    expect(held.admin.adoptReplacement(epoch!)).toBe(true);
    await held.admin.watchesSettled();
    expect(runs).toBe(2);
    expect(held.admin.replacedElsewhere()).toBeUndefined();
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
  expect(heard[0]).toEqual({ sequence: connection.lastCommit(), touched: 'all', source: 'local' });
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

it('notifies watches when a write outside a unit commits and throws NOT_DURABLE', async () => {
  e = engine({ wrap: committedFailure('write') });
  const w = watched(titles);
  await settled();
  expect(w.last()).toEqual([]);
  expect(() => e.create('Task', { title: 'kept-outside-unit' })).toThrow('the sync failed after commit');
  await settled();
  expect(w.last()).toEqual(['kept-outside-unit']);
  expect(w.runs()).toBe(2);
});

describe('closing the database during a watch flush', () => {
  const captureUncaught = async (run: () => Promise<void>) => {
    const uncaught: unknown[] = [];
    const onError = (error: unknown) => { uncaught.push(error); };
    process.on('uncaughtException', onError);
    try {
      await run();
      await new Promise((resolve) => setTimeout(resolve, 20));
    } finally {
      process.off('uncaughtException', onError);
    }
    return uncaught;
  };

  it('handles database.close() inside watch 1 read without uncaught CLOSED errors or leaked watches', async () => {
    let database: EarsDatabase | undefined;
    const local = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
    let secondRuns = 0;
    const errors: unknown[] = [];
    const uncaught = await captureUncaught(async () => {
      const w1 = local.watch(() => {
        database!.close();
        return local.query('Task').count();
      }, { next: () => {}, error: (err) => errors.push(err) });
      const w2 = local.watch(() => {
        secondRuns++;
        return local.query('Task').count();
      }, { next: () => {}, error: (err) => errors.push(err) });
      await Promise.all([w1.caughtUp(), w2.caughtUp(), local.admin.watchesSettled()]);
      expect(() => w1.stop()).not.toThrow();
      expect(() => w2.stop()).not.toThrow();
    });
    expect(uncaught).toEqual([]);
    expect(errors).toEqual([]);
    expect(secondRuns).toBe(0);
    expect(local.admin.watchStats().watches).toBe(0);
  });

  it('handles database.close() inside watch 1 observer without uncaught CLOSED errors or running watch 2', async () => {
    let database: EarsDatabase | undefined;
    const local = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
    let secondRuns = 0;
    const uncaught = await captureUncaught(async () => {
      const w1 = local.watch(() => local.query('Task').count(), () => { database!.close(); });
      const w2 = local.watch(() => { secondRuns++; return local.query('Task').count(); }, () => {});
      await Promise.all([w1.caughtUp(), w2.caughtUp(), local.admin.watchesSettled()]);
    });
    expect(uncaught).toEqual([]);
    expect(secondRuns).toBe(0);
    expect(local.admin.watchStats().watches).toBe(0);
  });

  it('allows stopping a watch after its database has already been closed', async () => {
    let database: EarsDatabase | undefined;
    const local = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
    const w = local.watch(() => local.query('Task').count(), () => {});
    await local.admin.watchesSettled();
    database!.close();
    expect(() => w.stop()).not.toThrow();
  });
});

describe('sameValue', () => {
  it('compares plain objects by their own enumerable keys alone, whichever comes first', () => {
    const hidden = Object.defineProperty({ y: 2 }, 'x', { value: 1, enumerable: false });
    expect(sameValue({ x: 1 }, hidden)).toBe(false);
    expect(sameValue(hidden, { x: 1 })).toBe(false);
    expect(sameValue({ y: 2 }, hidden)).toBe(true);
    expect(sameValue(hidden, { y: 2 })).toBe(true);
  });

  it('distinguishes typed array views with identical bytes', () => {
    expect(sameValue(new Uint8Array([0, 0, 0, 0]), new Float32Array([0]))).toBe(false);
    expect(sameValue(new Uint8Array([1, 2, 3]), new Uint8Array([1, 2, 3]))).toBe(true);
    expect(sameValue(new Uint8Array([1, 2, 3]), new Uint8Array([1, 2, 4]))).toBe(false);
  });

  it('treats two Invalid Dates as equal and distinct from valid Dates', () => {
    expect(sameValue(new Date(Number.NaN), new Date(Number.NaN))).toBe(true);
    expect(sameValue(new Date(Number.NaN), new Date(0))).toBe(false);
    expect(sameValue(new Date(1000), new Date(1000))).toBe(true);
  });

  it('does not treat distinct non-plain built-ins or class instances as equal empty plain objects', () => {
    expect(sameValue(/abc/g, /xyz/i)).toBe(false);
    expect(sameValue(/abc/g, /abc/g)).toBe(true);
    expect(sameValue(new URL('https://a.example'), new URL('https://b.example'))).toBe(false);
    expect(sameValue(new URL('https://a.example'), new URL('https://a.example'))).toBe(true);
    expect(sameValue(new Error('a'), new Error('b'))).toBe(false);
    expect(sameValue(new Error('same'), new Error('same'))).toBe(false);
    expect(sameValue(new ArrayBuffer(4), new ArrayBuffer(8))).toBe(false);
    const ab1 = new Uint8Array([1, 2, 3, 4]).buffer;
    const ab2 = new Uint8Array([1, 2, 3, 5]).buffer;
    const ab3 = new Uint8Array([1, 2, 3, 4]).buffer;
    expect(sameValue(ab1, ab2)).toBe(false);
    expect(sameValue(ab1, ab3)).toBe(true);
    expect(sameValue(new Set([1]), new Set([2]))).toBe(false);
    expect(sameValue(new Map([['a', 1]]), new Map([['a', 2]]))).toBe(false);
    class Box { #v: number; constructor(v: number) { this.#v = v; } value() { return this.#v; } }
    expect(sameValue(new Box(1), new Box(2))).toBe(false);
    expect(sameValue(new Box(1), new Box(1))).toBe(false);
  });

  it('compares cyclic object graphs safely without overflowing the stack', async () => {
    const a1: Record<string, unknown> = { x: 1 };
    a1.self = a1;
    const a2: Record<string, unknown> = { x: 1 };
    a2.self = a2;
    const b: Record<string, unknown> = { x: 2 };
    b.self = b;
    expect(sameValue(a1, a2)).toBe(true);
    expect(sameValue(a1, b)).toBe(false);

    const m1 = new Map<string, unknown>();
    m1.set('self', m1);
    const m2 = new Map<string, unknown>();
    m2.set('self', m2);
    expect(sameValue(m1, m2)).toBe(true);

    // End-to-end watch returning cyclic objects delivers once when structurally equal and again when changed
    const task = e.create('Task', { title: 'one' }).id();
    const w = watched(() => {
      const node: Record<string, unknown> = { title: e.getAttr(task, 'title') };
      node.self = node;
      return node;
    });
    await settled();
    expect(w.seen).toHaveLength(1);
    e.write(task).set('title', 'one');
    await settled();
    expect(w.seen, 'structurally equal cyclic value is suppressed').toHaveLength(1);
    e.write(task).set('title', 'two');
    await settled();
    expect(w.seen).toHaveLength(2);
    expect((w.last() as { title: string }).title).toBe('two');
    w.stop();
  });
});

describe('watch diagnostics, cause sources, builder sync and rolled-back writes', () => {
  it('exposes strictly payload-free watchDiagnostics with no entity values, IDs or query results', async () => {
    const secretId = e.create('Task', { title: 'SECRET_PAYLOAD_VALUE_99123', body: 'PRIVATE_NOTE_BODY' }).id();
    const w = e.watch(() => e.getRow(secretId), () => {}, { label: 'task-detail' });
    await settled();

    e.write(secretId).set('title', 'UPDATED_SECRET_PAYLOAD_88412');
    const waiting = w.caughtUp({ soon: true });
    const diag = e.admin.watchDiagnostics();
    const serialized = JSON.stringify(diag);
    expect(serialized).not.toContain(secretId);
    expect(serialized).not.toContain('SECRET_PAYLOAD');
    expect(serialized).not.toContain('PRIVATE_NOTE_BODY');
    expect(serialized).not.toContain('UPDATED_SECRET_PAYLOAD');
    expect(diag.scheduler.lastSeenCommit).toBe(e.lastCommit());
    expect(diag.scheduler.databaseCommit).toBe(e.lastCommit());
    expect(diag.watches).toHaveLength(1);
    expect(diag.watches[0]).toMatchObject({
      label: 'task-detail',
      owed: true,
      soon: true,
      waiters: 1,
    });
    await waiting;
    expect(e.admin.watchDiagnostics().watches[0]).toMatchObject({
      label: 'task-detail',
      owed: false,
      soon: false,
      waiters: 0,
      lastDeliveredCommit: e.lastCommit(),
    });
    w.stop();
  });

  it('reports payload-free cause sources for local, wildcard, elsewhere and builder commits', async () => {
    let database: EarsDatabase | undefined;
    const local = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
    const contexts: Array<import('../../src/index.ts').WatchReadContext | undefined> = [];
    const watching = local.watch(
      (ctx) => {
        contexts.push(ctx);
        return local.query('Task').ids().map(String);
      },
      () => {},
      { recordCauses: () => true, label: 'cause-sources' },
    );
    await local.admin.watchesSettled();
    expect(contexts[0]).toBeUndefined();

    // Targeted local write
    local.create('Task', { title: 'secret-local' });
    await local.admin.watchesSettled();
    expect(contexts[1]).toMatchObject({ sources: ['local'], wildcard: false, dropped: 0 });
    expect(JSON.stringify(contexts[1])).not.toContain('secret-local');

    // Wildcard local admin op (`clear`)
    local.admin.clear();
    await local.admin.watchesSettled();
    expect(contexts[2]).toMatchObject({ sources: ['local'], wildcard: true, dropped: 0 });

    // Elsewhere commit
    await commitElsewhere(database!, "create('Task', 'Task-remote')");
    expect(local.admin.changedElsewhere()).toBe(true);
    await local.admin.watchesSettled();
    expect(contexts[3]).toMatchObject({ sources: ['elsewhere'], wildcard: true, dropped: 0 });
    expect(JSON.stringify(contexts[3])).not.toContain('Task-remote');
    watching.stop();
  });

  it('syncs watches synchronously across waitForIndexBuilds so caughtUp reflects the built rank index', async () => {
    let database: EarsDatabase | undefined;
    const local = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
    const tasks: Id[] = Array.from({ length: 400 }, (_, at) => local.create('Task', { title: `t${at}` }).id());
    for (const task of tasks.slice(1)) local.write(task).link('refs', tasks[0]!);

    const contexts: Array<import('../../src/index.ts').WatchReadContext | undefined> = [];
    const seen: unknown[] = [];
    local.admin.declare({
      types: Object.fromEntries([...NAMES.types].map((type) => [type, { required: [] }])),
      relations: Object.fromEntries([...NAMES.relationKinds].map((kind) => [kind, {}])),
      roles: [...NAMES.roles],
      ranks: { taskRank: { types: ['Task'], relations: ['refs'] } },
    });
    const holding = database!.begin();
    const watching = local.watch(
      (ctx) => {
        contexts.push(ctx);
        return local.query('Task').ranked('taskRank', { whileBuilding: 'refuse' }).limit(1).ids();
      },
      {
        next: (answer) => seen.push(answer),
        error: (err) => seen.push((err as { code?: string }).code),
      },
      { recordCauses: () => true, label: 'ranked-watch' },
    );
    await watching.caughtUp({ soon: true });
    expect(seen).toEqual(['INDEX_BUILDING']);

    // Release the write lock, block synchronously until the builder finishes, then await caughtUp() immediately
    holding.abort();
    local.admin.waitForIndexBuilds();
    await watching.caughtUp({ soon: true });
    expect(seen.at(-1), 'caughtUp never resolves before the built rank index is delivered').toEqual([tasks[0]]);
    expect(contexts.at(-1)?.sources).toContain('builder');
    watching.stop();
  });

  it('produces zero watch runs, zero deliveries and zero sequence advance on rolled-back beforeCommit hooks and aborted transacts', async () => {
    let rejectCommit = false;
    e.admin.beforeCommit({
      wants: (type) => type === 'Task',
      run() {
        if (rejectCommit) throw new Error('hook rejected commit');
      },
    });
    const taskId = e.create('Task', { title: 'initial' }).id();
    const w = watched(() => e.query('Task').pick(['title']));
    await settled();
    const seqBefore = e.lastCommit();
    const runsBefore = w.runs();
    const seenBefore = w.seen.length;

    // 1. Aborted transact (throws after writes)
    expect(() => e.transact(() => {
      e.create('Task', { title: 'aborted-create' });
      e.write(taskId).set('title', 'aborted-update');
      throw new Error('abort unit');
    })).toThrow('abort unit');

    // 2. Rolled-back beforeCommit hook on standalone write and inside transact
    rejectCommit = true;
    expect(() => e.create('Task', { title: 'hook-rejected-create' })).toThrow('hook rejected commit');
    expect(() => e.write(taskId).set('title', 'hook-rejected-update')).toThrow('hook rejected commit');
    expect(() => e.transact(() => {
      e.write(taskId).set('title', 'hook-rejected-transact');
    })).toThrow('hook rejected commit');

    await settled();
    expect(e.lastCommit(), 'sequence must not advance on aborted or hook-rejected commits').toBe(seqBefore);
    expect(w.runs(), 'watch must not re-run on aborted or hook-rejected commits').toBe(runsBefore);
    expect(w.seen.length, 'watch must not deliver on aborted or hook-rejected commits').toBe(seenBefore);
    expect(w.last()).toEqual([{ id: taskId, title: 'initial' }]);
    w.stop();
  });
});
