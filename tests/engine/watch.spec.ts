// watch: a read runs again after each commit that wrote into what it read, and its listener hears each result that
// changed, after the writing code has returned, never inside a write. The guarantee that matters most — a watch never
// misses a change, whatever writes it — is the model-based harness's (model-based.spec.ts); these cases are the
// contract's edges, each with a reading of how many times a read ran where that is the point.
import { beforeEach, describe, expect, it } from 'vitest';
import { engine, type Id } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

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
