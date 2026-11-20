import { describe, expect, it } from 'vitest';
import { settles } from '../_support/waiting.ts';
import { engine, NAMES, type Id } from './helpers.ts';
import { freshEngine } from './engine-under-test.ts';
import type { EarsDatabase, EngineSchema } from '../../src/index.ts';

const RANKS = { taskRank: { types: ['Task'], relations: ['refs'] } };
const declared = (ranks?: EngineSchema['ranks']): EngineSchema => ({
  types: Object.fromEntries([...NAMES.types].map((type) => [type, { required: [] }])),
  relations: Object.fromEntries([...NAMES.relationKinds].map((kind) => [kind, {}])),
  roles: [...NAMES.roles],
  ...(ranks && { ranks }),
});

/** An engine with one rank index, and tasks all referring to the first but the first, which refers to the last */
function hubbed() {
  const e = engine({ ranks: RANKS });
  const tasks: Id[] = Array.from({ length: 6 }, (_, at) => e.create('Task', { title: `t${at}` }).id());
  const [hub] = tasks as [Id, ...Id[]];
  for (const task of tasks.slice(1)) e.write(task).link('refs', hub);
  e.write(hub).link('refs', tasks[5]!);
  e.admin.waitForIndexBuilds();
  return { e, tasks, hub };
}

describe('ranked', () => {
  it('orders a query by the index, highest first, with hits answering the scores rank computes', () => {
    const { e, tasks, hub } = hubbed();
    const ids = e.query('Task').ranked('taskRank').ids();
    expect(ids.slice(0, 2)).toEqual([hub, tasks[5]]);
    expect(new Set(ids)).toEqual(new Set(tasks));

    const exact = new Map(e.query('Task').rank('refs').hits().map((hit) => [hit.id, hit.score]));
    const hits = e.query('Task').ranked('taskRank').hits();
    expect(hits.map((hit) => hit.id)).toEqual(ids);
    for (const hit of hits) expect(hit.score).toBeCloseTo(exact.get(hit.id)!, 3);
  });

  it('keeps the top with limit, reading it from the index, whose bound explain answers', () => {
    const { e, tasks, hub } = hubbed();
    const explained = e.query('Task').ranked('taskRank').limit(2).explain();
    expect(explained.ids).toEqual([hub, tasks[5]]);
    const stage = explained.stages.find((at) => at.stage === 'ranked');
    expect(stage).toMatchObject({ index: 'taskRank' });
    expect(stage!.error).toBeGreaterThanOrEqual(0);
    expect(stage!.error).toBeLessThan(1e-3);
  });

  it('follows commits: a new hub takes the top once its pushes are done', () => {
    const { e, tasks } = hubbed();
    const riser = tasks[2]!;
    for (const task of tasks) if (task !== riser) e.write(task).unlink('refs', tasks[0]!);
    for (const task of tasks) if (task !== riser) e.write(task).link('refs', riser);
    e.admin.waitForIndexBuilds();
    expect(e.query('Task').ranked('taskRank').limit(1).ids()).toEqual([riser]);
  });

  it('refuses an index no schema declares, and a set outside its graph', () => {
    const { e } = hubbed();
    expect(() => e.query('Task').ranked('nothing').ids()).toThrow(expect.objectContaining({ code: 'UNKNOWN_INDEX' }));
    e.create('Project', { title: 'p' });
    expect(() => e.query('Project').ranked('taskRank').ids()).toThrow(expect.objectContaining({ code: 'BATCH_INVALID' }));
  });
});

describe('rankedHits', () => {
  it('answers a ranked\'s hits with its index and bound, as explain reports it', () => {
    const { e, hub } = hubbed();
    const { hits, bound } = e.query('Task').ranked('taskRank').limit(2).rankedHits();
    expect(hits[0]!.id).toBe(hub);
    expect(hits).toEqual(e.query('Task').ranked('taskRank').limit(2).hits());
    const stage = e.query('Task').ranked('taskRank').limit(2).explain().stages.find((at) => at.stage === 'ranked')!;
    expect(bound).toEqual({ index: 'taskRank', error: stage.error });
  });

  it('answers a rank\'s hits with its rounds, and picks fields as hits does', () => {
    const { e, hub } = hubbed();
    const { hits, bound } = e.query('Task').rank('refs').limit(1).rankedHits({ fields: ['title'] });
    expect(hits[0]).toMatchObject({ id: hub, row: { id: hub, title: 't0' } });
    expect(bound).toMatchObject({ converged: true });
    expect((bound as { iterations: number }).iterations).toBeGreaterThan(0);
  });

  it('refuses a query with neither, a search included, whose hits say nothing of a bound', () => {
    const { e } = hubbed();
    expect(() => e.query('Task').rankedHits()).toThrow(expect.objectContaining({ code: 'NO_SEARCH' }));
    expect(() => e.query('Task').ranked('taskRank').where('title', () => true).rankedHits()).toThrow(expect.objectContaining({ code: 'NO_SEARCH' }));
  });
});

describe('a rank index building', () => {
  /** An engine holding a graph with one hub and no rank index yet */
  function graph() {
    const e = engine();
    const tasks: Id[] = Array.from({ length: 2_000 }, (_, at) => e.create('Task', { title: `t${at}` }).id());
    const [hub] = tasks as [Id, ...Id[]];
    for (const task of tasks.slice(1)) e.write(task).link('refs', hub);
    return { e, hub };
  }

  it('answers as soon as it is declared, with the top and the scores a build will hold', () => {
    const { e, hub } = graph();
    e.admin.declare(declared(RANKS));
    expect(e.query('Task').ranked('taskRank').limit(1).ids()).toEqual([hub]);
    const [first] = e.query('Task').ranked('taskRank').limit(1).hits();
    const exact = e.query('Task').rank('refs').limit(1).hits()[0]!;
    expect(first!.id).toBe(hub);
    expect(first!.score).toBeCloseTo(exact.score, 3);
    e.admin.waitForIndexBuilds();
    expect(e.query('Task').ranked('taskRank').limit(1).ids()).toEqual([hub]);
    expect(e.query('Task').ranked('taskRank').limit(1).explain().stages.find((at) => at.stage === 'ranked')?.building).toBeUndefined();
  });

  it('refuses a read that asked to be, for as long as the build runs, and answers it after', () => {
    const { e, hub } = graph();
    const refusing = () => e.query('Task').ranked('taskRank', { whileBuilding: 'refuse' }).limit(1).ids();
    // The builder runs on its own thread and could finish first, so each try declares the index afresh
    const codes = Array.from({ length: 5 }, () => {
      e.admin.setSchema(declared());
      e.admin.setSchema(declared(RANKS));
      try { refusing(); return undefined; } catch (error) { return (error as { code?: string }).code; }
    });
    expect(codes).toContain('INDEX_BUILDING');
    e.admin.waitForIndexBuilds();
    expect(refusing()).toEqual([hub]);
  });

  it('never delivers a watch an error across the build, and leaves it on the index', async () => {
    const { e, hub } = graph();
    const answers: unknown[] = [];
    const errors: unknown[] = [];
    e.admin.declare(declared(RANKS));
    const watching = e.watch(() => e.query('Task').ranked('taskRank').limit(1).ids(), {
      next: (answer) => answers.push(answer),
      error: (error) => errors.push(error),
    });
    // The first read runs after this call returns, so it must get to run before the wait blocks the thread
    await settles(() => expect(answers.length + errors.length).toBeGreaterThan(0));
    e.admin.waitForIndexBuilds();
    await e.admin.watchesSettled();
    watching.stop();
    expect(errors).toEqual([]);
    expect(answers.at(-1)).toEqual([hub]);
  });
});

describe('a watch over a rank index', () => {
  it('runs again when the build ends, with nothing else writing, so one that refused recovers', async () => {
    let database: EarsDatabase | undefined;
    const e = freshEngine({ ...NAMES, wrap: (held) => (database = held) });
    const tasks: Id[] = Array.from({ length: 2_000 }, (_, at) => e.create('Task', { title: `t${at}` }).id());
    for (const task of tasks.slice(1)) e.write(task).link('refs', tasks[0]!);
    const seen: unknown[] = [];
    e.admin.declare(declared(RANKS));
    // Holds the write lock the build has to commit under, so it cannot finish before the watch's first read
    const holding = database!.begin();
    const watching = e.watch(() => e.query('Task').ranked('taskRank', { whileBuilding: 'refuse' }).limit(1).ids(), {
      next: (answer) => seen.push(answer),
      error: (error) => seen.push((error as { code?: string }).code),
    });
    await settles(() => expect(seen).toEqual(['INDEX_BUILDING']), { interval: 10 });
    holding.abort();
    await settles(() => expect(seen.at(-1)).toEqual([tasks[0]]));
    watching.stop();
  });
});

describe('declaring a rank index', () => {
  it('declare keeps those it does not name, and setSchema holds only those it names', () => {
    const { e, hub } = hubbed();
    e.admin.declare(declared({ projectRank: { types: ['Project'], relations: ['refs'] } }));
    e.admin.waitForIndexBuilds();
    expect(e.query('Task').ranked('taskRank').ids()[0]).toBe(hub);
    expect(e.query('Project').ranked('projectRank').ids()).toEqual([]);

    e.admin.setSchema(declared());
    expect(() => e.query('Task').ranked('taskRank').ids()).toThrow(expect.objectContaining({ code: 'UNKNOWN_INDEX' }));
  });
});
