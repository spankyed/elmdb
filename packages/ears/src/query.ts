import type { QueryAnswer, QuerySpec, Row } from 'elmdb';
import type { Hit } from 'elmdb/protocol/Hit.js';
import type { QueryFilter } from 'elmdb/protocol/QueryFilter.js';
import type { QueryScore } from 'elmdb/protocol/QueryScore.js';
import type { QuerySource } from 'elmdb/protocol/QuerySource.js';
import type { QueryStep } from 'elmdb/protocol/QueryStep.js';
import type { QueryTerminal } from 'elmdb/protocol/QueryTerminal.js';
import type { EarsDatabase, EdgeDirection, FusionMethod, SortDirection } from './database.ts';
import { EarsError, toEarsError } from './errors.ts';
import type { WatchHandle, WatchOptions, WatchPump } from './watch.ts';

/** Canonical entity row returned by `get` and row-returning queries. */
export type EntityRow<
  TName extends string = string,
  TFields extends Record<string, unknown> = Record<string, unknown>,
> = {
  readonly id: string;
  readonly typeName: TName;
  readonly trashed: boolean;
  readonly fields: Readonly<TFields>;
};

/** Options for source selectors (`of`, `ofTypes`, `allEntities`). */
export interface SourceOptions {
  readonly includeTrashed?: boolean;
}

/** Ergonomic field filter operator shorthand accepted by `QueryBuilder.where(field, predicate)`. */
export interface FieldPredicate {
  readonly eq?: unknown;
  readonly ne?: unknown;
  readonly in?: readonly unknown[];
  readonly gt?: unknown;
  readonly gte?: unknown;
  readonly lt?: unknown;
  readonly lte?: unknown;
  readonly startsWith?: string;
  readonly contains?: string;
  readonly regex?: string;
  readonly isNull?: boolean;
  readonly isPresent?: boolean;
}

/** Value or predicate shorthand accepted by `QueryBuilder.where(field, valueOrPredicate)`. */
export type WhereInput =
  | string
  | number
  | boolean
  | null
  | Uint8Array
  | FieldPredicate;

/** Options for `QueryBuilder.search(...)` (BM25F full-text search). */
export interface SearchOptions {
  readonly requireAll?: boolean;
  readonly phrase?: readonly string[];
  readonly topK?: number;
}

/** Options for `QueryBuilder.recency(...)`. */
export interface RecencyOptions {
  readonly nowMs: number;
  readonly halfLifeMs: number;
}

/** Options for `QueryBuilder.fuse(...)`. */
export interface FuseOptions {
  readonly method?: FusionMethod;
  readonly rrfK?: number;
}

/** Options for `QueryBuilder.hops(...)`. */
export interface HopsOptions {
  readonly relKinds?: readonly string[];
  readonly dir?: EdgeDirection;
  readonly minHops?: number;
  readonly maxHops: number;
}

/** Options for `QueryBuilder.shortestPath(...)`. */
export interface ShortestPathOptions {
  readonly relKinds?: readonly string[];
  readonly dir?: EdgeDirection;
  readonly maxHops: number;
}

/** Options for `QueryBuilder.hasLink(...)`. */
export interface HasLinkOptions {
  readonly relKinds?: readonly string[];
  readonly dir?: EdgeDirection;
  readonly targetId?: string;
}

/** Options for `QueryBuilder.orderBy(...)`. */
export interface OrderByOptions {
  readonly locale?: string;
}

/** Keyset pagination cursor for `QueryBuilder.after(...)`. */
export interface KeysetCursor {
  readonly id?: string;
  readonly score?: number;
  readonly value?: unknown;
}

/** Options for `QueryBuilder.ppr(...)` (Personalized PageRank). */
export interface PprOptions {
  readonly relKinds?: readonly string[];
  readonly weighted?: boolean;
  readonly damping?: number;
  readonly iterations?: number;
  readonly tolerance?: number;
}

/** Options for `QueryBuilder.leiden(...)` community detection. */
export interface LeidenOptions {
  readonly relKinds?: readonly string[];
  readonly weighted?: boolean;
  readonly resolution?: number;
  readonly seed?: number;
  readonly iterations?: number;
}

/** Options for `QueryBuilder.jaccard(...)` and `QueryBuilder.adamicAdar(...)`. */
export interface SimilarityOptions {
  readonly relKinds?: readonly string[];
  readonly dir?: EdgeDirection;
}

/** Options for `QueryBuilder.degrees(...)`. */
export interface DegreesOptions {
  readonly relKinds?: readonly string[];
  readonly dir?: EdgeDirection;
}

/** Result of `QueryBuilder.minMax(...)`. */
export interface MinMaxResult {
  readonly min: unknown | null;
  readonly max: unknown | null;
}

/** Result of `QueryBuilder.graphTopo()`. */
export interface GraphTopoResult {
  readonly order: string[];
  readonly cycles: number;
}

/** Result of `QueryBuilder.graphPpr()`. */
export interface GraphPprResult {
  readonly scores: Hit[];
  readonly converged: boolean;
  readonly iterations: number;
}

const PREDICATE_KEYS: Readonly<Record<keyof FieldPredicate, true>> = Object.freeze({
  eq: true,
  ne: true,
  in: true,
  gt: true,
  gte: true,
  lt: true,
  lte: true,
  startsWith: true,
  contains: true,
  regex: true,
  isNull: true,
  isPresent: true,
});

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value) || value instanceof Uint8Array) {
    return false;
  }
  const proto = Object.getPrototypeOf(value) as unknown;
  return proto === Object.prototype || proto === null;
}

function isFieldPredicate(value: unknown): value is FieldPredicate {
  if (!isPlainRecord(value)) return false;
  const keys = Object.keys(value);
  return keys.length > 0 && keys.every((k) => Object.prototype.hasOwnProperty.call(PREDICATE_KEYS, k));
}

function compileFieldPredicate(field: string, valueOrPredicate: WhereInput): QueryFilter {
  if (!isFieldPredicate(valueOrPredicate)) {
    return { op: 'eq', field, value: valueOrPredicate };
  }
  const filters: QueryFilter[] = [];
  if (valueOrPredicate.eq !== undefined) {
    filters.push({ op: 'eq', field, value: valueOrPredicate.eq });
  }
  if (valueOrPredicate.ne !== undefined) {
    filters.push({ op: 'ne', field, value: valueOrPredicate.ne });
  }
  if (valueOrPredicate.in !== undefined) {
    filters.push({ op: 'in', field, values: [...valueOrPredicate.in] });
  }
  if (
    valueOrPredicate.gt !== undefined
    || valueOrPredicate.gte !== undefined
    || valueOrPredicate.lt !== undefined
    || valueOrPredicate.lte !== undefined
  ) {
    const low: [unknown, boolean] | undefined =
      valueOrPredicate.gte !== undefined
        ? [valueOrPredicate.gte, true]
        : valueOrPredicate.gt !== undefined
          ? [valueOrPredicate.gt, false]
          : undefined;
    const high: [unknown, boolean] | undefined =
      valueOrPredicate.lte !== undefined
        ? [valueOrPredicate.lte, true]
        : valueOrPredicate.lt !== undefined
          ? [valueOrPredicate.lt, false]
          : undefined;
    filters.push({
      op: 'range',
      field,
      ...(low !== undefined && { low }),
      ...(high !== undefined && { high }),
    });
  }
  if (valueOrPredicate.startsWith !== undefined) {
    filters.push({ op: 'startsWith', field, prefix: valueOrPredicate.startsWith });
  }
  if (valueOrPredicate.contains !== undefined) {
    filters.push({ op: 'contains', field, needle: valueOrPredicate.contains });
  }
  if (valueOrPredicate.regex !== undefined) {
    filters.push({ op: 'regex', field, pattern: valueOrPredicate.regex });
  }
  if (valueOrPredicate.isNull !== undefined) {
    filters.push(
      valueOrPredicate.isNull
        ? { op: 'isNull', field }
        : { op: 'not', filter: { op: 'isNull', field } },
    );
  }
  if (valueOrPredicate.isPresent !== undefined) {
    filters.push(
      valueOrPredicate.isPresent
        ? { op: 'isPresent', field }
        : { op: 'not', filter: { op: 'isPresent', field } },
    );
  }
  if (filters.length === 0) {
    throw new EarsError('INVALID_ARGUMENT', `Empty filter predicate for field '${field}'`);
  }
  return filters.length === 1 ? filters[0]! : { op: 'and', filters };
}

/**
 * Immutable algebraic fluent query builder compiling 1:1 to elmdb v2 `QuerySpec`.
 *
 * Every combinator returns a new `QueryBuilder` so base queries can be safely shared and branched.
 */
export interface QueryBuilder<TResult extends EntityRow = EntityRow> {
  /** Adds a raw `QueryFilter` AST or a `where(field, valueOrPredicate)` filter step. */
  where(filter: QueryFilter): QueryBuilder<TResult>;
  where(field: string, valueOrPredicate: WhereInput): QueryBuilder<TResult>;
  /** Adds a negated filter step (`NOT filter`). */
  whereNot(filter: QueryFilter): QueryBuilder<TResult>;
  /** Adds a disjunctive filter step (`OR filters`). */
  whereAny(filters: readonly QueryFilter[]): QueryBuilder<TResult>;
  /** Adds a conjunctive filter step (`AND filters`). */
  whereAll(filters: readonly QueryFilter[]): QueryBuilder<TResult>;
  /** Filters to entities having a matching relation edge. */
  hasLink(opts?: HasLinkOptions): QueryBuilder<TResult>;

  /** BM25F full-text search scoring step. */
  search(index: string, query: string, opts?: SearchOptions): QueryBuilder<TResult>;
  /** HNSW approximate nearest-neighbor vector search scoring step. */
  near(index: string, vector: readonly number[], topK: number): QueryBuilder<TResult>;
  /** Vector similarity scoring seeded from an existing entity's embedding. */
  like(index: string, seedId: string, topK: number): QueryBuilder<TResult>;
  /** Materialized rank projection scoring step. */
  ranked(index: string): QueryBuilder<TResult>;
  /** Trigram + Levenshtein fuzzy label scoring step. */
  fuzzy(field: string, query: string, maxDistance: number): QueryBuilder<TResult>;
  /** Exponential recency decay scoring step over a timestamp/numeric field. */
  recency(field: string, opts: RecencyOptions): QueryBuilder<TResult>;
  /** Multi-signal score fusion (Reciprocal Rank Fusion or normalized linear blend). */
  fuse(inputs: ReadonlyArray<readonly [QueryScore, number]>, opts?: FuseOptions): QueryBuilder<TResult>;
  /** Appends an arbitrary `QueryScore` step. */
  score(spec: QueryScore): QueryBuilder<TResult>;

  /** Multi-hop BFS graph traversal (returns untyped `EntityRow` since hops can cross types). */
  hops(opts: HopsOptions): QueryBuilder<EntityRow>;
  /** Shortest-path traversal to `targetId`. */
  shortestPath(targetId: string, opts: ShortestPathOptions): QueryBuilder<EntityRow>;
  /** Deduplicates entities in the stream. */
  distinct(): QueryBuilder<TResult>;
  /** Personalized PageRank graph analytics step. */
  ppr(seeds: ReadonlyArray<readonly [string, number]>, opts?: PprOptions): QueryBuilder<EntityRow>;
  /** Weakly connected components graph analytics step. */
  weakComponents(relKinds?: readonly string[]): QueryBuilder<EntityRow>;
  /** Tarjan strongly connected components graph analytics step. */
  scc(relKinds?: readonly string[]): QueryBuilder<EntityRow>;
  /** Topological sort (with cycle count) graph analytics step. */
  topoSort(relKinds?: readonly string[]): QueryBuilder<EntityRow>;
  /** Leiden community detection graph analytics step. */
  leiden(opts?: LeidenOptions): QueryBuilder<EntityRow>;
  /** Top-K Jaccard neighborhood similarity graph step. */
  jaccard(seedId: string, topK: number, opts?: SimilarityOptions): QueryBuilder<EntityRow>;
  /** Top-K Adamic-Adar neighborhood similarity graph step. */
  adamicAdar(seedId: string, topK: number, opts?: SimilarityOptions): QueryBuilder<EntityRow>;

  /** Orders results by a field (with optional ICU collation `locale`). */
  orderBy(field: string, dir?: SortDirection, opts?: OrderByOptions): QueryBuilder<TResult>;
  /** Orders results by computed score (default `'desc'`). */
  orderByScore(dir?: SortDirection): QueryBuilder<TResult>;
  /** Orders results by external ID (default `'asc'`). */
  orderById(dir?: SortDirection): QueryBuilder<TResult>;
  /** Keyset pagination cursor applied to the current ordering. */
  after(cursor: KeysetCursor): QueryBuilder<TResult>;
  /** Caps the number of returned results. */
  limit(n: number): QueryBuilder<TResult>;

  /**
   * Explicit single-snapshot JavaScript post-filter applied to materialized rows (`all`/`first`/`watch`).
   * Refused on aggregate/non-row terminals (`count`/`ids`/`sum`/`minMax`/`groupBy`/`degrees`) so it
   * can never silently produce wrong aggregates.
   */
  filterLocal(predicate: (row: TResult) => boolean): QueryBuilder<TResult>;

  /** Compiles this builder into the wire `QuerySpec` without executing it. */
  toSpec(terminal?: QueryTerminal, explain?: boolean): QuerySpec;
  /** Executes this query with `explain: true` and returns the full `QueryAnswer` including its plan. */
  explain(terminal?: QueryTerminal): QueryAnswer;

  /** Materializes matching entity rows synchronously. */
  all(fields?: readonly string[]): TResult[];
  /** Materializes matching entity rows asynchronously on Node's worker pool. */
  allAsync(fields?: readonly string[]): Promise<TResult[]>;
  /** Materializes the first matching entity row (or `null`) synchronously. */
  first(fields?: readonly string[]): TResult | null;
  /** Materializes the first matching entity row (or `null`) asynchronously. */
  firstAsync(fields?: readonly string[]): Promise<TResult | null>;
  /** Returns matching entity IDs synchronously. */
  ids(): string[];
  /** Returns matching entity IDs asynchronously. */
  idsAsync(): Promise<string[]>;
  /** Returns scored `{ id, score }` hits synchronously. */
  hits(): Hit[];
  /** Returns scored `{ id, score }` hits asynchronously. */
  hitsAsync(): Promise<Hit[]>;
  /** Returns count of matching entities synchronously. */
  count(): number;
  /** Returns count of matching entities asynchronously. */
  countAsync(): Promise<number>;
  /** Returns whether any entity matches synchronously. */
  exists(): boolean;
  /** Returns whether any entity matches asynchronously. */
  existsAsync(): Promise<boolean>;
  /** Computes numeric sum of `field` synchronously. */
  sum(field: string): number;
  /** Computes numeric sum of `field` asynchronously. */
  sumAsync(field: string): Promise<number>;
  /** Computes `{ min, max }` of `field` synchronously. */
  minMax(field: string): MinMaxResult;
  /** Computes `{ min, max }` of `field` asynchronously. */
  minMaxAsync(field: string): Promise<MinMaxResult>;
  /** Groups matching entity IDs by `field` synchronously. */
  groupBy(field: string): Array<[unknown, string[]]>;
  /** Counts matching entities grouped by `field` synchronously. */
  groupCounts(field: string): Array<[unknown, number]>;
  /** Computes per-entity edge degrees synchronously. */
  degrees(opts?: DegreesOptions): Array<[string, number]>;
  /** Executes a community / component graph query (`leiden`, `scc`, `weakComponents`). */
  graphGroups(): string[][];
  /** Executes a `topoSort` graph query. */
  graphTopo(): GraphTopoResult;
  /** Executes a `ppr` graph query. */
  graphPpr(): GraphPprResult;

  /** Subscribes to live row updates driven by the engine's sans-I/O `WatchPump`. */
  watch(callback: (rows: TResult[]) => void, opts?: WatchOptions): WatchHandle;
  /** Subscribes to live ID list updates. */
  watchIds(callback: (ids: string[]) => void, opts?: WatchOptions): WatchHandle;
  /** Subscribes to live scored hit updates. */
  watchHits(callback: (hits: Hit[]) => void, opts?: WatchOptions): WatchHandle;
  /** Subscribes to live count updates. */
  watchCount(callback: (count: number) => void, opts?: WatchOptions): WatchHandle;
}

/** Root query entrypoints bound to an `EarsEngine`. */
export interface QueryRoot {
  of(typeName: string, opts?: SourceOptions): QueryBuilder<EntityRow>;
  ofTypes(typeNames: readonly string[], opts?: SourceOptions): QueryBuilder<EntityRow>;
  allEntities(opts?: SourceOptions): QueryBuilder<EntityRow>;
  ids(ids: readonly string[]): QueryBuilder<EntityRow>;
  raw(spec: QuerySpec): QueryAnswer;
  rawAsync(spec: QuerySpec): Promise<QueryAnswer>;
}

class QueryBuilderImpl<TResult extends EntityRow> implements QueryBuilder<TResult> {
  readonly #db: EarsDatabase;
  readonly #pump: WatchPump;
  readonly #source: QuerySource;
  readonly #steps: readonly QueryStep[];
  readonly #localFilters: ReadonlyArray<(row: TResult) => boolean>;

  constructor(
    db: EarsDatabase,
    pump: WatchPump,
    source: QuerySource,
    steps: readonly QueryStep[] = [],
    localFilters: ReadonlyArray<(row: TResult) => boolean> = [],
  ) {
    this.#db = db;
    this.#pump = pump;
    this.#source = source;
    this.#steps = steps;
    this.#localFilters = localFilters;
  }

  #withStep<TNext extends EntityRow = TResult>(step: QueryStep): QueryBuilderImpl<TNext> {
    return new QueryBuilderImpl<TNext>(
      this.#db,
      this.#pump,
      this.#source,
      [...this.#steps, step],
      this.#localFilters as unknown as ReadonlyArray<(row: TNext) => boolean>,
    );
  }

  #withUpdatedOrderSlice(
    patch: Partial<{
      by: string;
      field?: string;
      dir: string;
      locale?: string;
      afterId?: string;
      afterScore?: number;
      afterValue?: unknown | null;
      limit?: number;
    }>,
  ): QueryBuilderImpl<TResult> {
    const last = this.#steps[this.#steps.length - 1];
    if (last && last.kind === 'orderSlice') {
      const merged: QueryStep = {
        ...last,
        ...patch,
      };
      return new QueryBuilderImpl<TResult>(
        this.#db,
        this.#pump,
        this.#source,
        [...this.#steps.slice(0, -1), merged],
        this.#localFilters,
      );
    }
    const hasScore = this.#steps.some((s) => s.kind === 'score');
    const created: QueryStep = {
      kind: 'orderSlice',
      by: patch.by ?? (hasScore ? 'score' : 'id'),
      dir: patch.dir ?? (hasScore ? 'desc' : 'asc'),
      ...(patch.field !== undefined && { field: patch.field }),
      ...(patch.locale !== undefined && { locale: patch.locale }),
      ...(patch.afterId !== undefined && { afterId: patch.afterId }),
      ...(patch.afterScore !== undefined && { afterScore: patch.afterScore }),
      ...(patch.afterValue !== undefined && { afterValue: patch.afterValue }),
      ...(patch.limit !== undefined && { limit: patch.limit }),
    };
    return this.#withStep(created);
  }

  #forbidLocalFilter(terminalName: string): void {
    if (this.#localFilters.length > 0) {
      throw new EarsError(
        'INVALID_ARGUMENT',
        `filterLocal() cannot be combined with '${terminalName}()' because non-row terminals do not materialize rows for local filtering`,
      );
    }
  }

  #applyLocalFilters(rows: readonly Row[]): TResult[] {
    const typed = rows as unknown as TResult[];
    if (this.#localFilters.length === 0) return [...typed];
    return typed.filter((row) => this.#localFilters.every((fn) => fn(row)));
  }

  where(filterOrField: QueryFilter | string, valueOrPredicate?: WhereInput): QueryBuilder<TResult> {
    const expr =
      typeof filterOrField === 'string'
        ? compileFieldPredicate(filterOrField, valueOrPredicate as WhereInput)
        : filterOrField;
    return this.#withStep({ kind: 'filter', expr });
  }

  whereNot(filter: QueryFilter): QueryBuilder<TResult> {
    return this.#withStep({ kind: 'filter', expr: { op: 'not', filter } });
  }

  whereAny(filters: readonly QueryFilter[]): QueryBuilder<TResult> {
    return this.#withStep({ kind: 'filter', expr: { op: 'or', filters: [...filters] } });
  }

  whereAll(filters: readonly QueryFilter[]): QueryBuilder<TResult> {
    return this.#withStep({ kind: 'filter', expr: { op: 'and', filters: [...filters] } });
  }

  hasLink(opts?: HasLinkOptions): QueryBuilder<TResult> {
    return this.#withStep({
      kind: 'filter',
      expr: {
        op: 'hasLink',
        dir: opts?.dir ?? 'out',
        ...(opts?.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
        ...(opts?.targetId !== undefined && { targetId: opts.targetId }),
      },
    });
  }

  search(index: string, query: string, opts?: SearchOptions): QueryBuilder<TResult> {
    return this.score({
      kind: 'bm25f',
      index,
      query,
      ...(opts?.requireAll !== undefined && { requireAll: opts.requireAll }),
      ...(opts?.phrase !== undefined && { phrase: [...opts.phrase] }),
      ...(opts?.topK !== undefined && { topK: opts.topK }),
    });
  }

  near(index: string, vector: readonly number[], topK: number): QueryBuilder<TResult> {
    return this.score({ kind: 'vectorNear', index, vector: [...vector], topK });
  }

  like(index: string, seedId: string, topK: number): QueryBuilder<TResult> {
    return this.score({ kind: 'vectorLike', index, seedId, topK });
  }

  ranked(index: string): QueryBuilder<TResult> {
    return this.score({ kind: 'rankIndex', index });
  }

  fuzzy(field: string, query: string, maxDistance: number): QueryBuilder<TResult> {
    return this.score({ kind: 'fuzzyLabel', field, query, maxDistance });
  }

  recency(field: string, opts: RecencyOptions): QueryBuilder<TResult> {
    return this.score({
      kind: 'recencyDecay',
      field,
      nowMs: opts.nowMs,
      halfLifeMs: opts.halfLifeMs,
    });
  }

  fuse(inputs: ReadonlyArray<readonly [QueryScore, number]>, opts?: FuseOptions): QueryBuilder<TResult> {
    return this.score({
      kind: 'fuse',
      inputs: inputs.map(([s, w]) => [s, w]),
      ...(opts?.method !== undefined && { method: opts.method }),
      ...(opts?.rrfK !== undefined && { rrfK: opts.rrfK }),
    });
  }

  score(spec: QueryScore): QueryBuilder<TResult> {
    return this.#withStep({ kind: 'score', spec });
  }

  hops(opts: HopsOptions): QueryBuilder<EntityRow> {
    return this.#withStep<EntityRow>({
      kind: 'hops',
      dir: opts.dir ?? 'out',
      minHops: opts.minHops ?? 1,
      maxHops: opts.maxHops,
      ...(opts.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
    });
  }

  shortestPath(targetId: string, opts: ShortestPathOptions): QueryBuilder<EntityRow> {
    return this.#withStep<EntityRow>({
      kind: 'shortestPath',
      dir: opts.dir ?? 'out',
      targetId,
      maxHops: opts.maxHops,
      ...(opts.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
    });
  }

  distinct(): QueryBuilder<TResult> {
    return this.#withStep({ kind: 'distinct' });
  }

  ppr(seeds: ReadonlyArray<readonly [string, number]>, opts?: PprOptions): QueryBuilder<EntityRow> {
    return this.#withStep<EntityRow>({
      kind: 'graphPpr',
      seeds: seeds.map(([id, w]) => [id, w]),
      damping: opts?.damping ?? 0.85,
      iterations: opts?.iterations ?? 50,
      tolerance: opts?.tolerance ?? 1e-6,
      ...(opts?.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
      ...(opts?.weighted !== undefined && { weighted: opts.weighted }),
    });
  }

  weakComponents(relKinds?: readonly string[]): QueryBuilder<EntityRow> {
    return this.#withStep<EntityRow>({
      kind: 'graphWeakComponents',
      ...(relKinds !== undefined && { relKinds: [...relKinds] }),
    });
  }

  scc(relKinds?: readonly string[]): QueryBuilder<EntityRow> {
    return this.#withStep<EntityRow>({
      kind: 'graphScc',
      ...(relKinds !== undefined && { relKinds: [...relKinds] }),
    });
  }

  topoSort(relKinds?: readonly string[]): QueryBuilder<EntityRow> {
    return this.#withStep<EntityRow>({
      kind: 'graphTopoSort',
      ...(relKinds !== undefined && { relKinds: [...relKinds] }),
    });
  }

  leiden(opts?: LeidenOptions): QueryBuilder<EntityRow> {
    return this.#withStep<EntityRow>({
      kind: 'graphLeiden',
      resolution: opts?.resolution ?? 1.0,
      seed: opts?.seed ?? 42,
      iterations: opts?.iterations ?? 10,
      ...(opts?.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
      ...(opts?.weighted !== undefined && { weighted: opts.weighted }),
    });
  }

  jaccard(seedId: string, topK: number, opts?: SimilarityOptions): QueryBuilder<EntityRow> {
    return this.#withStep<EntityRow>({
      kind: 'graphJaccard',
      dir: opts?.dir ?? 'both',
      seedId,
      topK,
      ...(opts?.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
    });
  }

  adamicAdar(seedId: string, topK: number, opts?: SimilarityOptions): QueryBuilder<EntityRow> {
    return this.#withStep<EntityRow>({
      kind: 'graphAdamicAdar',
      dir: opts?.dir ?? 'both',
      seedId,
      topK,
      ...(opts?.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
    });
  }

  orderBy(field: string, dir: SortDirection = 'asc', opts?: OrderByOptions): QueryBuilder<TResult> {
    return this.#withStep({
      kind: 'orderSlice',
      by: 'field',
      field,
      dir,
      ...(opts?.locale !== undefined && { locale: opts.locale }),
    });
  }

  orderByScore(dir: SortDirection = 'desc'): QueryBuilder<TResult> {
    return this.#withStep({ kind: 'orderSlice', by: 'score', dir });
  }

  orderById(dir: SortDirection = 'asc'): QueryBuilder<TResult> {
    return this.#withStep({ kind: 'orderSlice', by: 'id', dir });
  }

  after(cursor: KeysetCursor): QueryBuilder<TResult> {
    return this.#withUpdatedOrderSlice({
      ...(cursor.id !== undefined && { afterId: cursor.id }),
      ...(cursor.score !== undefined && { afterScore: cursor.score }),
      ...(cursor.value !== undefined && { afterValue: cursor.value }),
    });
  }

  limit(n: number): QueryBuilder<TResult> {
    return this.#withUpdatedOrderSlice({ limit: n });
  }

  filterLocal(predicate: (row: TResult) => boolean): QueryBuilder<TResult> {
    return new QueryBuilderImpl<TResult>(
      this.#db,
      this.#pump,
      this.#source,
      this.#steps,
      [...this.#localFilters, predicate],
    );
  }

  toSpec(terminal?: QueryTerminal, explain?: boolean): QuerySpec {
    return {
      source: this.#source,
      ...(this.#steps.length > 0 && { steps: [...this.#steps] }),
      ...(terminal !== undefined && { terminal }),
      ...(explain !== undefined && { explain }),
    };
  }

  #run(terminal?: QueryTerminal, explain?: boolean): QueryAnswer {
    try {
      return this.#db.query(this.toSpec(terminal, explain));
    } catch (error) {
      throw toEarsError(error);
    }
  }

  async #runAsync(terminal?: QueryTerminal, explain?: boolean): Promise<QueryAnswer> {
    try {
      return await this.#db.queryAsync(this.toSpec(terminal, explain));
    } catch (error) {
      throw toEarsError(error);
    }
  }

  explain(terminal?: QueryTerminal): QueryAnswer {
    return this.#run(terminal ?? { kind: 'rows' }, true);
  }

  all(fields?: readonly string[]): TResult[] {
    const term: QueryTerminal = fields === undefined ? { kind: 'rows' } : { kind: 'rows', fields: [...fields] };
    const ans = this.#run(term);
    if (ans.kind !== 'rows') {
      throw new EarsError('INTERNAL', `Expected 'rows' query answer, got '${ans.kind}'`);
    }
    return this.#applyLocalFilters(ans.rows);
  }

  async allAsync(fields?: readonly string[]): Promise<TResult[]> {
    const term: QueryTerminal = fields === undefined ? { kind: 'rows' } : { kind: 'rows', fields: [...fields] };
    const ans = await this.#runAsync(term);
    if (ans.kind !== 'rows') {
      throw new EarsError('INTERNAL', `Expected 'rows' query answer, got '${ans.kind}'`);
    }
    return this.#applyLocalFilters(ans.rows);
  }

  first(fields?: readonly string[]): TResult | null {
    const rows = this.#localFilters.length === 0 ? this.limit(1).all(fields) : this.all(fields);
    return rows[0] ?? null;
  }

  async firstAsync(fields?: readonly string[]): Promise<TResult | null> {
    const rows = this.#localFilters.length === 0 ? await this.limit(1).allAsync(fields) : await this.allAsync(fields);
    return rows[0] ?? null;
  }

  ids(): string[] {
    this.#forbidLocalFilter('ids');
    const ans = this.#run({ kind: 'ids' });
    if (ans.kind !== 'ids') {
      throw new EarsError('INTERNAL', `Expected 'ids' query answer, got '${ans.kind}'`);
    }
    return ans.ids;
  }

  async idsAsync(): Promise<string[]> {
    this.#forbidLocalFilter('idsAsync');
    const ans = await this.#runAsync({ kind: 'ids' });
    if (ans.kind !== 'ids') {
      throw new EarsError('INTERNAL', `Expected 'ids' query answer, got '${ans.kind}'`);
    }
    return ans.ids;
  }

  hits(): Hit[] {
    this.#forbidLocalFilter('hits');
    const ans = this.#run({ kind: 'hits' });
    if (ans.kind !== 'hits') {
      throw new EarsError('INTERNAL', `Expected 'hits' query answer, got '${ans.kind}'`);
    }
    return ans.hits;
  }

  async hitsAsync(): Promise<Hit[]> {
    this.#forbidLocalFilter('hitsAsync');
    const ans = await this.#runAsync({ kind: 'hits' });
    if (ans.kind !== 'hits') {
      throw new EarsError('INTERNAL', `Expected 'hits' query answer, got '${ans.kind}'`);
    }
    return ans.hits;
  }

  count(): number {
    this.#forbidLocalFilter('count');
    const ans = this.#run({ kind: 'count' });
    if (ans.kind !== 'count') {
      throw new EarsError('INTERNAL', `Expected 'count' query answer, got '${ans.kind}'`);
    }
    return ans.count;
  }

  async countAsync(): Promise<number> {
    this.#forbidLocalFilter('countAsync');
    const ans = await this.#runAsync({ kind: 'count' });
    if (ans.kind !== 'count') {
      throw new EarsError('INTERNAL', `Expected 'count' query answer, got '${ans.kind}'`);
    }
    return ans.count;
  }

  exists(): boolean {
    this.#forbidLocalFilter('exists');
    const ans = this.#run({ kind: 'exists' });
    if (ans.kind !== 'exists') {
      throw new EarsError('INTERNAL', `Expected 'exists' query answer, got '${ans.kind}'`);
    }
    return ans.exists;
  }

  async existsAsync(): Promise<boolean> {
    this.#forbidLocalFilter('existsAsync');
    const ans = await this.#runAsync({ kind: 'exists' });
    if (ans.kind !== 'exists') {
      throw new EarsError('INTERNAL', `Expected 'exists' query answer, got '${ans.kind}'`);
    }
    return ans.exists;
  }

  sum(field: string): number {
    this.#forbidLocalFilter('sum');
    const ans = this.#run({ kind: 'sum', field });
    if (ans.kind !== 'sum') {
      throw new EarsError('INTERNAL', `Expected 'sum' query answer, got '${ans.kind}'`);
    }
    return ans.sum;
  }

  async sumAsync(field: string): Promise<number> {
    this.#forbidLocalFilter('sumAsync');
    const ans = await this.#runAsync({ kind: 'sum', field });
    if (ans.kind !== 'sum') {
      throw new EarsError('INTERNAL', `Expected 'sum' query answer, got '${ans.kind}'`);
    }
    return ans.sum;
  }

  minMax(field: string): MinMaxResult {
    this.#forbidLocalFilter('minMax');
    const ans = this.#run({ kind: 'minMax', field });
    if (ans.kind !== 'minMax') {
      throw new EarsError('INTERNAL', `Expected 'minMax' query answer, got '${ans.kind}'`);
    }
    return { min: ans.min, max: ans.max };
  }

  async minMaxAsync(field: string): Promise<MinMaxResult> {
    this.#forbidLocalFilter('minMaxAsync');
    const ans = await this.#runAsync({ kind: 'minMax', field });
    if (ans.kind !== 'minMax') {
      throw new EarsError('INTERNAL', `Expected 'minMax' query answer, got '${ans.kind}'`);
    }
    return { min: ans.min, max: ans.max };
  }

  groupBy(field: string): Array<[unknown, string[]]> {
    this.#forbidLocalFilter('groupBy');
    const ans = this.#run({ kind: 'groupBy', field, countsOnly: false });
    if (ans.kind !== 'groups') {
      throw new EarsError('INTERNAL', `Expected 'groups' query answer, got '${ans.kind}'`);
    }
    return ans.groups;
  }

  groupCounts(field: string): Array<[unknown, number]> {
    this.#forbidLocalFilter('groupCounts');
    const ans = this.#run({ kind: 'groupBy', field, countsOnly: true });
    if (ans.kind !== 'groupCounts') {
      throw new EarsError('INTERNAL', `Expected 'groupCounts' query answer, got '${ans.kind}'`);
    }
    return ans.groups;
  }

  degrees(opts?: DegreesOptions): Array<[string, number]> {
    this.#forbidLocalFilter('degrees');
    const ans = this.#run({
      kind: 'degrees',
      dir: opts?.dir ?? 'both',
      ...(opts?.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
    });
    if (ans.kind !== 'degrees') {
      throw new EarsError('INTERNAL', `Expected 'degrees' query answer, got '${ans.kind}'`);
    }
    return ans.degrees;
  }

  graphGroups(): string[][] {
    this.#forbidLocalFilter('graphGroups');
    const ans = this.#run();
    if (ans.kind !== 'graphGroups') {
      throw new EarsError('INTERNAL', `Expected 'graphGroups' query answer, got '${ans.kind}'`);
    }
    return ans.groups;
  }

  graphTopo(): GraphTopoResult {
    this.#forbidLocalFilter('graphTopo');
    const ans = this.#run();
    if (ans.kind !== 'graphTopo') {
      throw new EarsError('INTERNAL', `Expected 'graphTopo' query answer, got '${ans.kind}'`);
    }
    return { order: ans.order, cycles: ans.cycles };
  }

  graphPpr(): GraphPprResult {
    this.#forbidLocalFilter('graphPpr');
    const ans = this.#run();
    if (ans.kind !== 'graphPpr') {
      throw new EarsError('INTERNAL', `Expected 'graphPpr' query answer, got '${ans.kind}'`);
    }
    return { scores: ans.scores, converged: ans.converged, iterations: ans.iterations };
  }

  watch(callback: (rows: TResult[]) => void, opts?: WatchOptions): WatchHandle {
    const spec = this.toSpec({ kind: 'rows' });
    const localFilters = this.#localFilters;
    return this.#pump.register(
      spec,
      (answer) => {
        if (answer.kind !== 'rows') {
          throw new EarsError('INTERNAL', `Expected 'rows' watch answer, got '${answer.kind}'`);
        }
        const typed = answer.rows as unknown as TResult[];
        callback(localFilters.length === 0 ? [...typed] : typed.filter((r) => localFilters.every((fn) => fn(r))));
      },
      opts,
    );
  }

  watchIds(callback: (ids: string[]) => void, opts?: WatchOptions): WatchHandle {
    this.#forbidLocalFilter('watchIds');
    return this.#pump.register(
      this.toSpec({ kind: 'ids' }),
      (answer) => {
        if (answer.kind !== 'ids') {
          throw new EarsError('INTERNAL', `Expected 'ids' watch answer, got '${answer.kind}'`);
        }
        callback(answer.ids);
      },
      opts,
    );
  }

  watchHits(callback: (hits: Hit[]) => void, opts?: WatchOptions): WatchHandle {
    this.#forbidLocalFilter('watchHits');
    return this.#pump.register(
      this.toSpec({ kind: 'hits' }),
      (answer) => {
        if (answer.kind !== 'hits') {
          throw new EarsError('INTERNAL', `Expected 'hits' watch answer, got '${answer.kind}'`);
        }
        callback(answer.hits);
      },
      opts,
    );
  }

  watchCount(callback: (count: number) => void, opts?: WatchOptions): WatchHandle {
    this.#forbidLocalFilter('watchCount');
    return this.#pump.register(
      this.toSpec({ kind: 'count' }),
      (answer) => {
        if (answer.kind !== 'count') {
          throw new EarsError('INTERNAL', `Expected 'count' watch answer, got '${answer.kind}'`);
        }
        callback(answer.count);
      },
      opts,
    );
  }
}

/** Creates the `QueryRoot` bound to an `EarsDatabase` and `WatchPump`. */
export function createQueryRoot(db: EarsDatabase, pump: WatchPump): QueryRoot {
  return {
    of(typeName, opts) {
      return new QueryBuilderImpl(db, pump, {
        kind: 'type',
        typeName,
        ...(opts?.includeTrashed !== undefined && { includeTrashed: opts.includeTrashed }),
      });
    },
    ofTypes(typeNames, opts) {
      return new QueryBuilderImpl(db, pump, {
        kind: 'types',
        typeNames: [...typeNames],
        ...(opts?.includeTrashed !== undefined && { includeTrashed: opts.includeTrashed }),
      });
    },
    allEntities(opts) {
      return new QueryBuilderImpl(db, pump, {
        kind: 'allEntities',
        ...(opts?.includeTrashed !== undefined && { includeTrashed: opts.includeTrashed }),
      });
    },
    ids(ids) {
      return new QueryBuilderImpl(db, pump, { kind: 'ids', ids: [...ids] });
    },
    raw(spec) {
      try {
        return db.query(spec);
      } catch (error) {
        throw toEarsError(error);
      }
    },
    async rawAsync(spec) {
      try {
        return await db.queryAsync(spec);
      } catch (error) {
        throw toEarsError(error);
      }
    },
  };
}
