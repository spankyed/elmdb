// The engine's queries: a builder chain compiled to one of elmdb's plans (`elmdb/docs/architecture/engine.md`, Reads: plans), which a
// terminal sends and decodes the answer of.
import type { EARS } from './entities.ts';
import type { EdgeDirection, QueryBuilder, QueryStage, RelationEdge, SimilarityMeasure } from './runtime.ts';
import { EarsError } from './errors.ts';
import type { Answer } from './database.ts';
import type { TypeName } from './write.ts';
import type { NameChecks } from './names.ts';

type MaybeArr<T> = T | readonly T[];
const asArr = <T>(v: MaybeArr<T>): T[] => (Array.isArray(v) ? [...v] : [v]) as T[];

export const b64Encode = (n: number) => btoa(String(n));
export const b64Decode = (s: string) => {
  try {
    const decoded = atob(s);
    const num = parseInt(decoded, 10);
    return isNaN(num) ? 0 : num;
  } catch {
    return 0;
  }
};

/**
 * The query entry points over the entities they reach. `(type)` starts from every entity of a registered type, and
 * refuses a name that isn't one rather than finding nothing; `ids` starts from the ids given that exist; `all` from
 * every entity. A relation is not an entity: `findRelations` reads them.
 */
export interface QueryEntries {
  <E extends string>(type: TypeName<E>): QueryBuilder<E>;
  <E extends string>(types: readonly TypeName<E>[]): QueryBuilder<string>;
  ids<E extends string>(ids: EARS.EntityId<E> | readonly EARS.EntityId<E>[]): QueryBuilder<E>;
  all(): QueryBuilder<string>;
}

/**
 * The engine's queries. They reach no entity in the trash — not as a start, not at a link's far end — and
 * `withTrashed` is the same entry points reaching those too; `trashed` the same entry points starting from only the
 * trashed entities.
 */
export interface Query extends QueryEntries {
  withTrashed: QueryEntries;
  trashed: QueryEntries;
}

/** How many rows an unindexed `where` filters before the engine's creator is told (`warn`) the field wants an index */
export const UNINDEXED_WALK_WARNING = 2_000;

/** Which entities a plan reaches (elmdb's `Scope`) */
type Scope = 'live' | 'withTrashed' | 'trashed';
type Start = { from: 'types'; types: string[] } | { from: 'ids'; ids: string[] } | { from: 'all' };
type Step = { step: string } & Record<string, unknown>;
type Terminal = { answer: string } & Record<string, unknown>;

/**
 * A step not yet sent: one of elmdb's, or a `where` holding the caller's function, which can't cross. A plan holding
 * one is sent up to it, answering each entity's value of the field, and goes on from the ids the function admits
 */
type Pending = { step: Step } | { field: string; test: (value: unknown) => boolean };

interface Recipe {
  scope: Scope;
  start: Start;
  steps: readonly Pending[];
  /** The `where` straight after a start from types whose field one of them doesn't index: what `warn` is told of */
  unindexed?: { types: string[]; field: string };
}

export interface QueryScope {
  /** Answers reads, in the unit `transact` holds open or a snapshot of their own */
  read(reads: ReadonlyArray<Record<string, unknown>>): Answer[];
  isEntityType(name: string): boolean;
  /** Whether a `where` on the type's field reads a value index */
  indexed(type: string, field: string): boolean;
  names: NameChecks;
  warn?: (message: string) => void;
}

/** A relation row as a read answers it: `info` is left out when the edge holds none */
export function relationRow(row: Record<string, unknown>): RelationEdge {
  const { info, ...rest } = row;
  return (info === null || info === undefined ? rest : { ...rest, info }) as unknown as RelationEdge;
}

/** The terminals answered by the first id a plan reaches */
const FIRST: readonly string[] = ['first', 'exists', 'firstRow'];
/** The steps that keep or walk the ids one at a time, so the first id of their answer comes from the first ids they're given */
const STREAMING: readonly string[] = [
  'ofType', 'within', 'where', 'has', 'lacks', 'withRole', 'linkedWith', 'linkedTo', 'linkedFrom', 'unlinkedTo', 'unlinkedFrom', 'out', 'in', 'both', 'limit',
];
/** The steps whose hits `hits()` answers */
const SEARCHES: readonly string[] = ['search', 'near', 'like', 'hybrid', 'rank', 'similar'];

/**
 * A count of edges as elmdb takes it (a `u32`): rounded down, `0` for a negative count or one that isn't a number, and
 * for an infinite count, or one past what a `u32` holds, nothing sent, so any number
 */
const edgeCount = (name: string, n: number | undefined) =>
  n === undefined || n >= 2 ** 32 ? {} : { [name]: Number.isNaN(n) ? 0 : Math.max(0, Math.floor(n)) };

/** Whether a value is one a `where` sends to elmdb, which matches it with `===` or as an element of a list */
const sendable = (v: unknown) =>
  v === null || typeof v === 'string' || typeof v === 'boolean' || (typeof v === 'number' && Number.isFinite(v));

export function createQuery({ read, isEntityType, indexed, names, warn }: QueryScope): Query {
  /** The types a start names; refuses a name that isn't a registered type */
  const ofTypes = (start: unknown): string[] => {
    const types = Array.isArray(start) ? start as unknown[] : [start];
    const unknown = types.filter((t) => typeof t !== 'string' || !isEntityType(t));
    if (unknown.length) {
      throw new EarsError(
        'UNKNOWN_ENTITY_TYPE',
        `query(${unknown.map((t) => JSON.stringify(t) ?? 'undefined').join(', ')}): not a registered entity type. ` +
          'query.ids(...) starts from ids, and query.all() from every entity.',
      );
    }
    return [...new Set(types as string[])];
  };

  /**
   * Tells the engine's creator, once per call site, that a `where` filtered a type's rows for want of an index: the
   * site is the first frame outside this module's directory (`src/` or `dist/`, or the bundle it was built into), so
   * the message names the caller's code. The directory is read off this frame rather than `import.meta`, which a
   * CommonJS bundle leaves empty
   */
  const warnedSites = new Set<string>();
  const ownDir = /\(?((?:file:\/\/)?[^()\s]*?)[\\/][^\\/]+:\d+:\d+\)?$/.exec(new Error().stack?.split('\n')[1]?.trim() ?? '')?.[1];
  const own = (frame: string) => ownDir !== undefined && frame.includes(ownDir);
  const callSite = () => {
    // Deep enough to get past the engine's own frames
    const limit = Error.stackTraceLimit;
    Error.stackTraceLimit = 50;
    const stack = new Error().stack;
    Error.stackTraceLimit = limit;
    return stack?.split('\n').slice(1).find((frame) => !own(frame))?.trim() ?? 'unknown';
  };
  const unindexed = (type: string, field: string, walked: number, site: string) => {
    const key = `${type}\x1F${field}\x1F${site}`;
    if (!warn || warnedSites.has(key)) return;
    warnedSites.add(key);
    warn(`where('${field}') filtered ${walked} ${type} rows without an index (${site}). `
      + `Declare it in the ${type} entry of apack.json: "indexes": ["${field}"]`);
  };

  /**
   * How many of the ids a caller's function admits the rest of a plan can use, when that is fewer than all: the rest
   * is limits alone, and its terminal answers from the first, so the function stops being called once that many pass
   */
  const wanted = (rest: readonly Pending[], terminal: Terminal): number => {
    if (!rest.every((p) => 'step' in p && p.step.step === 'limit')) return Infinity;
    const limits = rest.map((p) => ('step' in p ? p.step.n as number : Infinity));
    const first = FIRST.includes(terminal.answer) ? 1 : Infinity;
    return Math.min(first, ...limits);
  };

  /** Runs the recipe to its terminal: the plans before each caller's function, then the one after the last */
  const run = (recipe: Recipe, terminal: Terminal): Answer => {
    let { scope, start } = recipe;
    let steps: Step[] = [];
    // The first plan answers how many rows its walk reached, which says whether an unindexed `where` should be reported
    let visits = warn !== undefined && recipe.unindexed !== undefined;
    const send = (plan: Record<string, unknown>): Answer => {
      if (!visits) return read([{ read: 'query', plan }])[0]!;
      visits = false;
      const [{ answer, visited }] = read([{ read: 'query', plan, visits: true }]) as Array<Answer & { answer: Answer; visited: number }>;
      if (visited >= UNINDEXED_WALK_WARNING) unindexed(recipe.unindexed!.types.join('/'), recipe.unindexed!.field, visited, callSite());
      return answer;
    };
    for (const [at, pending] of recipe.steps.entries()) {
      if ('step' in pending) {
        steps.push(pending.step);
        continue;
      }
      const rows = send({ scope, start, steps, terminal: { answer: 'rows', fields: [pending.field] } }).rows as Record<string, unknown>[];
      const passes = (row: Record<string, unknown>) =>
        Object.prototype.hasOwnProperty.call(row, pending.field) && pending.test(row[pending.field]);
      const rest = recipe.steps.slice(at + 1);
      // The ids kept are each one the scope reached, so the rest reaches what the scope's steps do: the trash too,
      // for a query from it
      scope = scope === 'trashed' ? 'withTrashed' : scope;
      steps = [];
      if (FIRST.includes(terminal.answer) && rest.every((p) => 'step' in p && STREAMING.includes(p.step.step))) {
        // An answer from the first id the rest reaches: each admitted id is tried in turn, until one reaches one
        const restSteps = rest.map((p) => (p as { step: Step }).step);
        for (const row of rows) {
          if (!passes(row)) continue;
          const answer = send({ scope, start: { from: 'ids', ids: [row.id] }, steps: restSteps, terminal });
          if (answer.id != null || answer.value === true || answer.row != null) return answer;
        }
        return send({ scope, start: { from: 'ids', ids: [] }, steps: restSteps, terminal });
      }
      const enough = wanted(rest, terminal);
      const kept: string[] = [];
      for (const row of rows) {
        if (kept.length >= enough) break;
        if (passes(row)) kept.push(row.id as string);
      }
      start = { from: 'ids', ids: kept };
    }
    return send({ scope, start, steps, terminal });
  };

  const builder = (recipe: Recipe): QueryBuilder<string> => {
    const next = (step: Step, unindexedWhere?: Recipe['unindexed']): QueryBuilder<string> =>
      builder({ ...recipe, steps: [...recipe.steps, { step }], ...(unindexedWhere && { unindexed: unindexedWhere }) });
    const atStart = recipe.steps.length === 0 && recipe.start.from === 'types' ? recipe.start.types : undefined;
    const ids = () => run(recipe, { answer: 'ids' }).ids as EARS.EntityId[];
    const walk = (via: string, step: string, kinds: MaybeArr<string>, type?: MaybeArr<string>, depth?: number) => {
      names.relationKinds(kinds, via);
      return next({ step, kinds: asArr(kinds), ...(type !== undefined && { types: asArr(type) }), ...edgeCount('depth', depth) });
    };
    const filterByLink = (step: string, kind: string, other?: EARS.EntityId) => {
      names.relationKinds(kind, step);
      return next({ step, kind, ...(other !== undefined && { other }) });
    };
    const edges = (via: string, kinds: MaybeArr<string>, type: MaybeArr<string> | undefined, direction: 'out' | 'in') => {
      names.relationKinds(kinds, via);
      const answer = run(recipe, { answer: 'edges', kinds: asArr(kinds), direction, ...(type !== undefined && { types: asArr(type) }) });
      return (answer.relations as Record<string, unknown>[]).map(relationRow);
    };

    const self = {
      ofType: (t: string) => next({ step: 'ofType', type: t }),
      within: (sub: readonly EARS.EntityId[]) => next({ step: 'within', ids: [...sub] }),
      where: (k: string, v: unknown) => {
        if (v === undefined) {
          throw new EarsError('NO_VALUE', `where('${k}'): no value to compare with. has('${k}') and lacks('${k}') ask whether there is one`);
        }
        if (typeof v === 'function') {
          return builder({ ...recipe, steps: [...recipe.steps, { field: k, test: v as (value: unknown) => boolean }] });
        }
        // A value only `===` itself matches, an object or a Date, matches nothing a read gives back
        if (!sendable(v)) return builder({ ...recipe, steps: [...recipe.steps, { field: k, test: (value) => value === v }] });
        const walked = atStart?.filter((type) => !indexed(type, k));
        return next({ step: 'where', field: k, value: v }, walked?.length ? { types: walked, field: k } : undefined);
      },
      has: (k: string) => next({ step: 'has', field: k }),
      lacks: (k: string) => next({ step: 'lacks', field: k }),
      withRole: (r: string) => {
        names.role(r, 'withRole');
        return next({ step: 'withRole', role: r });
      },
      linkedWith: (other: EARS.EntityId) => next({ step: 'linkedWith', other }),
      linkedTo: (kind: string, other?: EARS.EntityId) => filterByLink('linkedTo', kind, other),
      linkedFrom: (kind: string, other?: EARS.EntityId) => filterByLink('linkedFrom', kind, other),
      unlinkedTo: (kind: string, other?: EARS.EntityId) => filterByLink('unlinkedTo', kind, other),
      unlinkedFrom: (kind: string, other?: EARS.EntityId) => filterByLink('unlinkedFrom', kind, other),
      out: (kinds: MaybeArr<string>, type?: MaybeArr<string>) => walk('out', 'out', kinds, type),
      in: (kinds: MaybeArr<string>, type?: MaybeArr<string>) => walk('in', 'in', kinds, type),
      both: (kinds: MaybeArr<string>, type?: MaybeArr<string>) => walk('both', 'both', kinds, type),
      descendants: (kinds: MaybeArr<string>, type?: MaybeArr<string>, { depth }: { depth?: number } = {}) =>
        walk('descendants', 'descendants', kinds, type, depth),
      ancestors: (kinds: MaybeArr<string>, type?: MaybeArr<string>, { depth }: { depth?: number } = {}) =>
        walk('ancestors', 'ancestors', kinds, type, depth),
      pathTo: (to: EARS.EntityId, kinds: MaybeArr<string>, { direction = 'out', depth }: { direction?: EdgeDirection; depth?: number } = {}) => {
        names.relationKinds(kinds, 'pathTo');
        return next({ step: 'pathTo', to, kinds: asArr(kinds), direction, ...edgeCount('depth', depth) });
      },
      outEdges: (kinds: MaybeArr<string>, type?: MaybeArr<string>) => edges('outEdges', kinds, type, 'out'),
      inEdges: (kinds: MaybeArr<string>, type?: MaybeArr<string>) => edges('inEdges', kinds, type, 'in'),
      degrees: (kinds: MaybeArr<string>, type?: MaybeArr<string>, { direction = 'out' }: { direction?: EdgeDirection } = {}) => {
        names.relationKinds(kinds, 'degrees');
        const answer = run(recipe, { answer: 'degrees', kinds: asArr(kinds), direction, ...(type !== undefined && { types: asArr(type) }) });
        return new Map(answer.degrees as Array<[EARS.EntityId, number]>);
      },
      rank: (kinds: MaybeArr<string>, { direction = 'out', seeds }: { direction?: EdgeDirection; seeds?: readonly EARS.EntityId[] } = {}) => {
        names.relationKinds(kinds, 'rank');
        return next({ step: 'rank', kinds: asArr(kinds), direction, ...(seeds !== undefined && { seeds: [...seeds] }) });
      },
      topological: (kinds: MaybeArr<string>, { direction = 'out' }: { direction?: 'out' | 'in' } = {}) => {
        names.relationKinds(kinds, 'topological');
        return next({ step: 'topological', kinds: asArr(kinds), direction });
      },
      similar: (
        to: EARS.EntityId,
        kinds: MaybeArr<string>,
        { direction = 'out', measure = 'jaccard' }: { direction?: EdgeDirection; measure?: SimilarityMeasure } = {},
      ) => {
        names.relationKinds(kinds, 'similar');
        return next({ step: 'similar', to, kinds: asArr(kinds), direction, measure });
      },
      weakComponents: (kinds: MaybeArr<string>) => {
        names.relationKinds(kinds, 'weakComponents');
        return run(recipe, { answer: 'weakComponents', kinds: asArr(kinds) }).weakComponents as EARS.EntityId[][];
      },
      strongComponents: (kinds: MaybeArr<string>) => {
        names.relationKinds(kinds, 'strongComponents');
        return run(recipe, { answer: 'strongComponents', kinds: asArr(kinds) }).strongComponents as EARS.EntityId[][];
      },
      communities: (kinds: MaybeArr<string>, { resolution, seed }: { resolution?: number; seed?: number } = {}) => {
        names.relationKinds(kinds, 'communities');
        const answer = run(recipe, {
          answer: 'communities', kinds: asArr(kinds), ...(resolution !== undefined && { resolution }), ...(seed !== undefined && { seed }),
        });
        return answer.communities as EARS.EntityId[][];
      },

      pick: (fields: readonly string[]) => run(recipe, { answer: 'rows', fields: [...fields] }).rows,
      // The first row: the fields picked, or with none the whole row, as pickAll reads it
      pickOne: (fields?: readonly string[]) =>
        run(recipe, { answer: 'firstRow', ...(fields && { fields: [...fields] }) }).row ?? undefined,
      pickAll: () => run(recipe, { answer: 'rows' }).rows,

      orderBy: (field: string, dir: 'asc' | 'desc' = 'asc') => next({ step: 'orderBy', field, descending: dir === 'desc' }),
      reverse: () => next({ step: 'reverse' }),
      // The first `n` ids, `n` rounded down: none for a count below one, or one that isn't a number, and every one for
      // an infinite count
      limit: (n: number) => (n === Infinity ? self : next({ step: 'limit', n: Number.isNaN(n) ? 0 : Math.max(0, Math.floor(n)) })),
      distinct: (field?: string) => next({ step: 'distinct', ...(field !== undefined && { field }) }),
      groupBy: (field: string) => new Map(run(recipe, { answer: 'groupBy', field }).groups as Array<[unknown, EARS.EntityId[]]>),

      search: (text: string, options: Record<string, unknown> = {}) => next({ step: 'search', query: text, options }),
      containing: (text: string, { caseInsensitive = false }: { caseInsensitive?: boolean } = {}) =>
        next({ step: 'containing', text, caseInsensitive }),
      matching: (pattern: string) => next({ step: 'matching', pattern }),
      after: (id: EARS.EntityId) => next({ step: 'after', id }),
      near: (vector: readonly number[], { k = 10 }: { k?: number } = {}) => next({ step: 'near', vector: [...vector], k }),
      like: (id: EARS.EntityId, { k = 10 }: { k?: number } = {}) => next({ step: 'like', id, k }),
      hybrid: (text: string, vector: readonly number[], { k = 10, ...options }: Record<string, unknown> & { k?: number } = {}) =>
        next({ step: 'hybrid', query: text, vector: [...vector], k, options }),
      hits: ({ fields, highlights = false }: { fields?: readonly string[]; highlights?: boolean } = {}) => {
        // The search must be in the plan the terminal ends, after every function a `where` was given
        const lastWhere = (test: (p: Pending) => boolean) => recipe.steps.reduce((last, p, at) => (test(p) ? at : last), -1);
        const searched = lastWhere((p) => 'step' in p && SEARCHES.includes(p.step.step as string));
        const split = lastWhere((p) => !('step' in p));
        if (searched < 0 || split > searched) {
          throw new EarsError('NO_SEARCH', searched < 0
            ? 'hits() answers what a search, a rank or a similar found: the query has no search(...), near(...), like(...), hybrid(...), rank(...) or similar(...)'
            : 'hits() answers what a search, a rank or a similar found, and a where(...) given a function follows it: put the search, rank or similar after it');
        }
        const answer = run(recipe, { answer: 'hits', highlights, ...(fields && { fields: [...fields] }) });
        return answer.hits;
      },

      ids,
      explain: () => {
        if (recipe.steps.some((p) => !('step' in p))) {
          throw new EarsError('NOT_EXPLAINABLE', 'explain() answers the plan the database runs, and a where(...) given a function runs outside it: explain the query up to that where');
        }
        const plan = { scope: recipe.scope, start: recipe.start, steps: recipe.steps.map((p) => (p as { step: Step }).step), terminal: { answer: 'ids' } };
        const [explained] = read([{ read: 'query', plan, explain: true }]) as Array<Answer & { answer: Answer; visited: number; ms: number; stages: QueryStage[] }>;
        return { ids: explained!.answer.ids as EARS.EntityId[], visited: explained!.visited, ms: explained!.ms, stages: explained!.stages };
      },
      count: () => run(recipe, { answer: 'count' }).n as number,
      first: () => (run(recipe, { answer: 'first' }).id as EARS.EntityId | null) ?? undefined,
      last: () => (run(recipe, { answer: 'last' }).id as EARS.EntityId | null) ?? undefined,
      exists: () => run(recipe, { answer: 'exists' }).value as boolean,

      // A callback runs on the ids once they are read, so its own writes are its to read
      map: <T>(fn: (i: EARS.EntityId) => T) => ids().map(fn),
      forEach: (fn: (i: EARS.EntityId) => void) => (ids().forEach(fn), self),
      reduce: <T>(fn: (a: T, i: EARS.EntityId) => T, init: T) => ids().reduce(fn, init),
    };
    return self as unknown as QueryBuilder<string>;
  };

  /** The entry points over a scope */
  const entriesOver = (scope: Scope): QueryEntries => {
    const entries = ((types: unknown) => builder({ scope, start: { from: 'types', types: ofTypes(types) }, steps: [] })) as unknown as QueryEntries;
    // The ids named: each checked when the query runs, so one that has left the engine (or the trash) is skipped
    entries.ids = ((given: EARS.EntityId | readonly EARS.EntityId[]) =>
      builder({ scope, start: { from: 'ids', ids: Array.isArray(given) ? [...given] : [given as EARS.EntityId] }, steps: [] })) as QueryEntries['ids'];
    entries.all = (() => builder({ scope, start: { from: 'all' }, steps: [] })) as QueryEntries['all'];
    return entries;
  };

  const entry = entriesOver('live') as Query;
  entry.withTrashed = entriesOver('withTrashed');
  entry.trashed = entriesOver('trashed');
  return entry;
}
