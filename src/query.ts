// The engine's queries: a builder chain compiled to one of elmdb's plans (`elmdb/docs/architecture/engine.md`, Reads: plans), which a
// terminal sends and decodes the answer of. Each step and terminal is written whole as a literal of elmdb's generated
// type, so a field renamed there fails to compile here.
import type { Plan } from 'elmdb/protocol/Plan.js';
import type { ReadOp } from 'elmdb/protocol/ReadOp.js';
import type { Scope } from 'elmdb/protocol/Scope.js';
import type { SearchOptions as ElmdbSearchOptions } from 'elmdb/protocol/SearchOptions.js';
import type { Start } from 'elmdb/protocol/Start.js';
import type { Step } from 'elmdb/protocol/Step.js';
import type { Terminal } from 'elmdb/protocol/Terminal.js';
import type { EARS } from './entities.ts';
import type { EARSRuntimeDeps, EdgeDirection, QueryBuilder, QueryStage, RankedOptions, RankOptions, RelationEdge, SearchOptions, SimilarityMeasure } from './runtime.ts';
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
  read(reads: readonly ReadOp[]): Answer[];
  isEntityType(name: string): boolean;
  /** Whether a `where` on the type's field reads a value index */
  indexed(type: string, field: string): boolean;
  names: NameChecks;
  warn?: EARSRuntimeDeps['warn'];
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
const SEARCHES: readonly string[] = ['search', 'near', 'like', 'hybrid', 'rank', 'ranked', 'similar'];
/** The steps whose scores `rankedHits()` can say something of */
const RANKS: readonly string[] = ['rank', 'ranked'];

/**
 * A count of edges as elmdb takes it (a `u32`): rounded down, `0` for a negative count or one that isn't a number, and
 * for an infinite count, or one past what a `u32` holds, none, so any number
 */
const edgeCount = (n: number | undefined): number | undefined =>
  n === undefined || n >= 2 ** 32 ? undefined : Number.isNaN(n) ? 0 : Math.max(0, Math.floor(n));

/** A list given as one or many, as elmdb takes it; none when none is given */
const optionalArr = <T>(v: MaybeArr<T> | undefined): T[] | undefined => (v === undefined ? undefined : asArr(v));

/** The steps of pending ones that are elmdb's: those before or after a `where` given a function */
const stepsOf = (pending: readonly Pending[]): Step[] => pending.flatMap((p) => ('step' in p ? [p.step] : []));

/** Search options as elmdb takes them, each named, so a field renamed there fails to compile here */
const searchOptions = ({ prefix, fuzzy, any, fields, recency }: SearchOptions = {}): ElmdbSearchOptions => ({
  prefix,
  fuzzy,
  any,
  fields: fields && [...fields],
  recency: recency && { field: recency.field, halfLife: recency.halfLife, now: recency.now },
});

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
  const unindexed = (types: string[], field: string, visited: number, site: string) => {
    const type = types.join('/');
    const key = `${type}\x1F${field}\x1F${site}`;
    if (!warn || warnedSites.has(key)) return;
    warnedSites.add(key);
    warn(`where('${field}') filtered ${visited} ${type} rows without an index (${site}). `
      + `Declare a value index on ${type}.${field}: "indexes": ["${field}"] in its row rules`,
      { kind: 'unindexedWalk', types, field, visited, site });
  };

  /**
   * How many of the ids a caller's function admits the rest of a plan can use, when that is fewer than all: the rest
   * is limits alone, and its terminal answers from the first, so the function stops being called once that many pass
   */
  const wanted = (rest: readonly Pending[], terminal: Terminal): number => {
    if (!rest.every((p) => 'step' in p && p.step.step === 'limit')) return Infinity;
    const limits = rest.map((p) => ('step' in p && p.step.step === 'limit' ? p.step.n : Infinity));
    const first = FIRST.includes(terminal.answer) ? 1 : Infinity;
    return Math.min(first, ...limits);
  };

  /** Runs the recipe to its terminal: the plans before each caller's function, then the one after the last */
  const run = (recipe: Recipe, terminal: Terminal): Answer => {
    let { scope, start } = recipe;
    let steps: Step[] = [];
    // The first plan answers how many rows its walk reached, which says whether an unindexed `where` should be reported
    let visits = warn !== undefined && recipe.unindexed !== undefined;
    const send = (plan: Plan): Answer => {
      if (!visits) return read([{ read: 'query', plan }])[0]!;
      visits = false;
      const answers = read([{ read: 'query', plan, visits: true }]);
      const [{ answer, visited }] = answers as Array<Answer & { answer: Answer; visited: number }>;
      if (visited >= UNINDEXED_WALK_WARNING) unindexed(recipe.unindexed!.types, recipe.unindexed!.field, visited, callSite());
      return answer;
    };
    for (const [at, pending] of recipe.steps.entries()) {
      if ('step' in pending) {
        steps.push(pending.step);
        continue;
      }
      const values = send({ scope, start, steps, terminal: { answer: 'rows', fields: [pending.field] } });
      const rows = values.rows as Array<Record<string, unknown> & { id: string }>;
      const passes = (row: Record<string, unknown>) =>
        Object.prototype.hasOwnProperty.call(row, pending.field) && pending.test(row[pending.field]);
      const rest = recipe.steps.slice(at + 1);
      // The ids kept are each one the scope reached, so the rest reaches what the scope's steps do: the trash too,
      // for a query from it
      scope = scope === 'trashed' ? 'withTrashed' : scope;
      steps = [];
      if (FIRST.includes(terminal.answer) && rest.every((p) => 'step' in p && STREAMING.includes(p.step.step))) {
        // An answer from the first id the rest reaches: each admitted id is tried in turn, until one reaches one
        const restSteps = stepsOf(rest);
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
        if (passes(row)) kept.push(row.id);
      }
      start = { from: 'ids', ids: kept };
    }
    return send({ scope, start, steps, terminal });
  };

  class Builder {
    readonly #recipe: Recipe;
    #methods?: Map<string, (...args: unknown[]) => unknown>;
    constructor(recipe: Recipe) {
      this.#recipe = recipe;
    }

    #next(step: Step, unindexedWhere?: Recipe['unindexed']): QueryBuilder<string> {
      return builder({ ...this.#recipe, steps: [...this.#recipe.steps, { step }], ...(unindexedWhere && { unindexed: unindexedWhere }) });
    }
    /** A walk one edge from each entity */
    #walk(step: 'out' | 'in' | 'both', kinds: MaybeArr<string>, type?: MaybeArr<string>) {
      names.relationKinds(kinds, step);
      return this.#next({ step, kinds: asArr(kinds), types: optionalArr(type) });
    }
    /** A walk any number of edges deep, `depth` at most */
    #deepWalk(step: 'descendants' | 'ancestors', kinds: MaybeArr<string>, type?: MaybeArr<string>, depth?: number) {
      names.relationKinds(kinds, step);
      return this.#next({ step, kinds: asArr(kinds), types: optionalArr(type), depth: edgeCount(depth) });
    }
    #filterByLink(step: 'linkedTo' | 'linkedFrom' | 'unlinkedTo' | 'unlinkedFrom', kind: string, other?: EARS.EntityId) {
      names.relationKinds(kind, step);
      return this.#next({ step, kind, other });
    }
    #edges(via: string, kinds: MaybeArr<string>, type: MaybeArr<string> | undefined, direction: 'out' | 'in') {
      names.relationKinds(kinds, via);
      const answer = run(this.#recipe, { answer: 'edges', kinds: asArr(kinds), direction, types: optionalArr(type) });
      return (answer.relations as Record<string, unknown>[]).map(relationRow);
    }
    #ids() {
      const answer = run(this.#recipe, { answer: 'ids' });
      return answer.ids as EARS.EntityId[];
    }

    ofType(t: string) { return this.#next({ step: 'ofType', type: t }); }
    within(sub: readonly EARS.EntityId[]) { return this.#next({ step: 'within', ids: [...sub] }); }
    where(k: string, v: unknown) {
      if (v === undefined) {
        throw new EarsError('NO_VALUE', `where('${k}'): no value to compare with. has('${k}') and lacks('${k}') ask whether there is one`);
      }
      if (typeof v === 'function') {
        return builder({ ...this.#recipe, steps: [...this.#recipe.steps, { field: k, test: v as (value: unknown) => boolean }] });
      }
      // A value only `===` itself matches, an object or a Date, matches nothing a read gives back
      if (!sendable(v)) return builder({ ...this.#recipe, steps: [...this.#recipe.steps, { field: k, test: (value) => value === v }] });
      const atStart = this.#recipe.steps.length === 0 && this.#recipe.start.from === 'types' ? this.#recipe.start.types : undefined;
      const walked = atStart?.filter((type) => !indexed(type, k));
      return this.#next({ step: 'where', field: k, value: v }, walked?.length ? { types: walked, field: k } : undefined);
    }
    has(k: string) { return this.#next({ step: 'has', field: k }); }
    lacks(k: string) { return this.#next({ step: 'lacks', field: k }); }
    withRole(r: string) {
      names.role(r, 'withRole');
      return this.#next({ step: 'withRole', role: r });
    }
    linkedWith(other: EARS.EntityId) { return this.#next({ step: 'linkedWith', other }); }
    linkedTo(kind: string, other?: EARS.EntityId) { return this.#filterByLink('linkedTo', kind, other); }
    linkedFrom(kind: string, other?: EARS.EntityId) { return this.#filterByLink('linkedFrom', kind, other); }
    unlinkedTo(kind: string, other?: EARS.EntityId) { return this.#filterByLink('unlinkedTo', kind, other); }
    unlinkedFrom(kind: string, other?: EARS.EntityId) { return this.#filterByLink('unlinkedFrom', kind, other); }
    out(kinds: MaybeArr<string>, type?: MaybeArr<string>) { return this.#walk('out', kinds, type); }
    in(kinds: MaybeArr<string>, type?: MaybeArr<string>) { return this.#walk('in', kinds, type); }
    both(kinds: MaybeArr<string>, type?: MaybeArr<string>) { return this.#walk('both', kinds, type); }
    descendants(kinds: MaybeArr<string>, type?: MaybeArr<string>, { depth }: { depth?: number; } = {}) { return this.#deepWalk('descendants', kinds, type, depth); }
    ancestors(kinds: MaybeArr<string>, type?: MaybeArr<string>, { depth }: { depth?: number; } = {}) { return this.#deepWalk('ancestors', kinds, type, depth); }
    pathTo(to: EARS.EntityId, kinds: MaybeArr<string>, { direction = 'out', depth }: { direction?: EdgeDirection; depth?: number; } = {}) {
      names.relationKinds(kinds, 'pathTo');
      return this.#next({ step: 'pathTo', to, kinds: asArr(kinds), direction, depth: edgeCount(depth) });
    }
    outEdges(kinds: MaybeArr<string>, type?: MaybeArr<string>) { return this.#edges('outEdges', kinds, type, 'out'); }
    inEdges(kinds: MaybeArr<string>, type?: MaybeArr<string>) { return this.#edges('inEdges', kinds, type, 'in'); }
    degrees(kinds: MaybeArr<string>, type?: MaybeArr<string>, { direction = 'out' }: { direction?: EdgeDirection; } = {}) {
      names.relationKinds(kinds, 'degrees');
      const answer = run(this.#recipe, { answer: 'degrees', kinds: asArr(kinds), direction, types: optionalArr(type) });
      return new Map(answer.degrees as Array<[EARS.EntityId, number]>);
    }
    ranked(index: string, { whileBuilding }: RankedOptions = {}) { return this.#next({ step: 'ranked', index, whileBuilding }); }
    rank(kinds: MaybeArr<string>, { direction = 'out', seeds, weights }: RankOptions = {}) {
      names.relationKinds(kinds, 'rank');
      return this.#next({ step: 'rank', kinds: asArr(kinds), direction, seeds: seeds && [...seeds], weights });
    }
    topological(kinds: MaybeArr<string>, { direction = 'out' }: { direction?: 'out' | 'in'; } = {}) {
      names.relationKinds(kinds, 'topological');
      return this.#next({ step: 'topological', kinds: asArr(kinds), direction });
    }
    similar(to: EARS.EntityId, kinds: MaybeArr<string>, { direction = 'out', measure = 'jaccard' }: { direction?: EdgeDirection; measure?: SimilarityMeasure; } = {}) {
      names.relationKinds(kinds, 'similar');
      return this.#next({ step: 'similar', to, kinds: asArr(kinds), direction, measure });
    }
    weakComponents(kinds: MaybeArr<string>) {
      names.relationKinds(kinds, 'weakComponents');
      const answer = run(this.#recipe, { answer: 'weakComponents', kinds: asArr(kinds) });
      return answer.weakComponents as EARS.EntityId[][];
    }
    strongComponents(kinds: MaybeArr<string>) {
      names.relationKinds(kinds, 'strongComponents');
      const answer = run(this.#recipe, { answer: 'strongComponents', kinds: asArr(kinds) });
      return answer.strongComponents as EARS.EntityId[][];
    }
    communities(kinds: MaybeArr<string>, { resolution, seed, weights }: { resolution?: number; seed?: number; weights?: boolean; } = {}) {
      names.relationKinds(kinds, 'communities');
      const answer = run(this.#recipe, { answer: 'communities', kinds: asArr(kinds), resolution, seed, weights });
      return answer.communities as EARS.EntityId[][];
    }
    pick(fields: readonly string[]) { return run(this.#recipe, { answer: 'rows', fields: [...fields] }).rows; }
    // The first row: the fields picked, or with none the whole row, as pickAll reads it
    pickOne(fields?: readonly string[]) { return run(this.#recipe, { answer: 'firstRow', fields: fields && [...fields] }).row ?? undefined; }
    pickAll() { return run(this.#recipe, { answer: 'rows' }).rows; }
    orderBy(field: string, dir: 'asc' | 'desc' = 'asc') { return this.#next({ step: 'orderBy', field, descending: dir === 'desc' }); }
    reverse() { return this.#next({ step: 'reverse' }); }
    // The first `n` ids, `n` rounded down: none for a count below one, or one that isn't a number, and every one for
    // an infinite count
    limit(n: number) { return (n === Infinity ? this : this.#next({ step: 'limit', n: Number.isNaN(n) ? 0 : Math.max(0, Math.floor(n)) })); }
    distinct(field?: string) { return this.#next({ step: 'distinct', field }); }
    groupBy(field: string) {
      const answer = run(this.#recipe, { answer: 'groupBy', field });
      return new Map(answer.groups as Array<[unknown, EARS.EntityId[]]>);
    }
    search(text: string, options?: SearchOptions) { return this.#next({ step: 'search', query: text, options: searchOptions(options) }); }
    containing(text: string, { caseInsensitive = false }: { caseInsensitive?: boolean; } = {}) { return this.#next({ step: 'containing', text, caseInsensitive }); }
    matching(pattern: string) { return this.#next({ step: 'matching', pattern }); }
    after(id: EARS.EntityId) { return this.#next({ step: 'after', id }); }
    near(vector: readonly number[], { k = 10 }: { k?: number; } = {}) { return this.#next({ step: 'near', vector: [...vector], k }); }
    like(id: EARS.EntityId, { k = 10 }: { k?: number; } = {}) { return this.#next({ step: 'like', id, k }); }
    hybrid(text: string, vector: readonly number[], { k = 10, ...options }: SearchOptions & { k?: number; } = {}) { return this.#next({ step: 'hybrid', query: text, vector: [...vector], k, options: searchOptions(options) }); }
    hits({ fields, highlights = false }: { fields?: readonly string[]; highlights?: boolean; } = {}) {
      this.#requireStep('hits', SEARCHES, 'search, a rank or a similar', 'search(...), near(...), like(...), hybrid(...), rank(...), ranked(...) or similar(...)');
      return run(this.#recipe, { answer: 'hits', highlights, fields: fields && [...fields] }).hits;
    }
    rankedHits({ fields }: { fields?: readonly string[]; } = {}) {
      this.#requireStep('rankedHits', RANKS, 'rank or a ranked', 'rank(...) or ranked(...)');
      const answer = run(this.#recipe, { answer: 'hits', highlights: false, fields: fields && [...fields] });
      return { hits: answer.hits, bound: answer.rank };
    }
    /** Refuses (`NO_SEARCH`) a terminal whose step isn't in the plan it ends, after every function a `where` was given */
    #requireStep(terminal: string, steps: readonly string[], what: string, named: string) {
      const lastWhere = (test: (p: Pending) => boolean) => this.#recipe.steps.reduce((last, p, at) => (test(p) ? at : last), -1);
      const found = lastWhere((p) => 'step' in p && steps.includes(p.step.step));
      const split = lastWhere((p) => !('step' in p));
      if (found < 0) throw new EarsError('NO_SEARCH', `${terminal}() answers what a ${what} found: the query has no ${named}`);
      if (split > found) throw new EarsError('NO_SEARCH', `${terminal}() answers what a ${what} found, and a where(...) given a function follows it: put it after that`);
    }
    ids() { return this.#ids(); }
    explain() {
      if (this.#recipe.steps.some((p) => !('step' in p))) {
        throw new EarsError('NOT_EXPLAINABLE', 'explain() answers the plan the database runs, and a where(...) given a function runs outside it: explain the query up to that where');
      }
      const plan: Plan = { scope: this.#recipe.scope, start: this.#recipe.start, steps: stepsOf(this.#recipe.steps), terminal: { answer: 'ids' } };
      const answers = read([{ read: 'query', plan, explain: true }]);
      const [explained] = answers as Array<Answer & { answer: Answer; visited: number; ms: number; stages: QueryStage[]; }>;
      return { ids: explained!.answer.ids as EARS.EntityId[], visited: explained!.visited, ms: explained!.ms, stages: explained!.stages };
    }
    count() {
      const answer = run(this.#recipe, { answer: 'count' });
      return answer.n as number;
    }
    first() {
      const answer = run(this.#recipe, { answer: 'first' });
      return (answer.id as EARS.EntityId | null) ?? undefined;
    }
    last() {
      const answer = run(this.#recipe, { answer: 'last' });
      return (answer.id as EARS.EntityId | null) ?? undefined;
    }
    exists() {
      const answer = run(this.#recipe, { answer: 'exists' });
      return answer.value as boolean;
    }
    // A callback runs on the ids once they are read, so its own writes are its to read
    map<T>(fn: (i: EARS.EntityId) => T) { return this.#ids().map(fn); }
    forEach(fn: (i: EARS.EntityId) => void) { return (this.#ids().forEach(fn), this); }
    reduce<T>(fn: (a: T, i: EARS.EntityId) => T, init: T) { return this.#ids().reduce(fn, init); }
    // Share implementations, binding only methods a recipe uses. Private state also permits a
    // frozen recipe to retain stable extracted functions without caching any database answers.
    static {
      for (const name of Object.getOwnPropertyNames(Builder.prototype)) {
        if (name === 'constructor') continue;
        const method = Object.getOwnPropertyDescriptor(Builder.prototype, name)!.value as (...args: unknown[]) => unknown;
        Object.defineProperty(Builder.prototype, name, {
          configurable: true,
          get(this: Builder) {
            const methods = this.#methods ??= new Map();
            let bound = methods.get(name);
            if (!bound) methods.set(name, bound = method.bind(this));
            return bound;
          },
          set(this: Builder, value: unknown) {
            Object.defineProperty(this, name, { value, writable: true, configurable: true, enumerable: true });
          },
        });
      }
    }
  }

  const builder = (recipe: Recipe): QueryBuilder<string> => new Builder(recipe) as unknown as QueryBuilder<string>;

  /** The entry points over a scope */
  const entriesOver = (scope: Scope): QueryEntries => {
    const from = (start: Start) => builder({ scope, start, steps: [] });
    const types = (given: unknown) => from({ from: 'types', types: ofTypes(given) });
    // The ids named: each checked when the query runs, so one that has left the engine (or the trash) is skipped
    const ids = (given: EARS.EntityId | readonly EARS.EntityId[]) => from({ from: 'ids', ids: typeof given === 'string' ? [given] : [...given] });
    const all = () => from({ from: 'all' });
    return Object.assign(types, { ids, all }) as unknown as QueryEntries;
  };

  const entry = entriesOver('live') as Query;
  entry.withTrashed = entriesOver('withTrashed');
  entry.trashed = entriesOver('trashed');
  return entry;
}
