import type { QueryAnswer, QuerySpec, Row } from 'elmdb';
import type { Hit } from 'elmdb/protocol/Hit.js';
import type { QueryFilter } from 'elmdb/protocol/QueryFilter.js';
import type { QueryScore } from 'elmdb/protocol/QueryScore.js';
import type { QuerySource } from 'elmdb/protocol/QuerySource.js';
import type { QueryStep } from 'elmdb/protocol/QueryStep.js';
import type { QueryTerminal } from 'elmdb/protocol/QueryTerminal.js';
import type { EarsDatabase, EdgeDirection, FusionMethod, SortDirection } from './database.ts';
import { EarsError, toEarsError } from './errors.ts';
import type { EarsOperationObserver } from './observer.ts';
import type { WatchHandle, WatchOptions, WatchPump } from './watch.ts';

const TYPED_ARRAY_PROTO = Object.getPrototypeOf(Uint8Array.prototype) as object;
const TYPED_ARRAY_TAG_GETTER = Object.getOwnPropertyDescriptor(TYPED_ARRAY_PROTO, Symbol.toStringTag)?.get;
const TYPED_ARRAY_BUFFER_GETTER = Object.getOwnPropertyDescriptor(TYPED_ARRAY_PROTO, 'buffer')?.get;
const TYPED_ARRAY_BYTE_OFFSET_GETTER = Object.getOwnPropertyDescriptor(TYPED_ARRAY_PROTO, 'byteOffset')?.get;
const TYPED_ARRAY_BYTE_LENGTH_GETTER = Object.getOwnPropertyDescriptor(TYPED_ARRAY_PROTO, 'byteLength')?.get;

function asRealUint8Array(value: unknown): Uint8Array | null {
  if (
    value === null
    || typeof value !== 'object'
    || !TYPED_ARRAY_TAG_GETTER
    || !TYPED_ARRAY_BUFFER_GETTER
    || !TYPED_ARRAY_BYTE_OFFSET_GETTER
    || !TYPED_ARRAY_BYTE_LENGTH_GETTER
  ) {
    return null;
  }
  try {
    if (TYPED_ARRAY_TAG_GETTER.call(value) !== 'Uint8Array') return null;
    const buffer = TYPED_ARRAY_BUFFER_GETTER.call(value) as ArrayBufferLike;
    const byteOffset = TYPED_ARRAY_BYTE_OFFSET_GETTER.call(value) as number;
    const byteLength = TYPED_ARRAY_BYTE_LENGTH_GETTER.call(value) as number;
    return new Uint8Array(buffer, byteOffset, byteLength);
  } catch {
    return null;
  }
}

function hasLoneSurrogate(s: string): boolean {
  for (let i = 0; i < s.length; i++) {
    const code = s.charCodeAt(i);
    if (code >= 0xd800 && code <= 0xdbff) {
      const next = i + 1 < s.length ? s.charCodeAt(i + 1) : 0;
      if (next >= 0xdc00 && next <= 0xdfff) {
        i++;
      } else {
        return true;
      }
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      return true;
    }
  }
  return false;
}

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
  | Date
  | FieldPredicate;

/** Options for `QueryBuilder.search(...)` (BM25F full-text search). */
export interface SearchOptions {
  readonly requireAll?: boolean;
  readonly prefix?: boolean;
  readonly phrase?: readonly string[];
  readonly topK?: number;
}

/** Options for `QueryBuilder.hits(...)` and `QueryBuilder.hitsAsync(...)`. */
export interface HitsOptions {
  readonly fields?: readonly string[];
  readonly highlights?: boolean;
}

/** Options for `QueryBuilder.watchHits(...)`. */
export interface WatchHitsOptions extends WatchOptions, HitsOptions {}

/** Options for `QueryBuilder.watch(...)`. */
export interface WatchRowsOptions extends WatchOptions {
  /** Optional list of field names to project on delivered rows. */
  readonly fields?: readonly string[];
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

/** Keyset pagination cursor for `QueryBuilder.after(...)`. `id` is required as the tie-breaker key. */
export interface KeysetCursor {
  readonly id: string;
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

const GRAPH_ANALYZE_STEP_KINDS: Readonly<Record<string, true>> = Object.freeze({
  graphPpr: true,
  graphWeakComponents: true,
  graphScc: true,
  graphTopoSort: true,
  graphLeiden: true,
  graphJaccard: true,
  graphAdamicAdar: true,
});

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  if (
    typeof value !== 'object'
    || value === null
    || Array.isArray(value)
    || value instanceof Uint8Array
    || value instanceof Date
  ) {
    return false;
  }
  const proto = Object.getPrototypeOf(value) as unknown;
  return proto === Object.prototype || proto === null;
}

const MAX_QUERY_VALUE_DEPTH = 64;

function normalizeQueryValue(
  value: unknown,
  seen: Set<object> = new Set(),
  depth = 0,
): unknown {
  if (value === null || typeof value === 'boolean' || typeof value === 'string') {
    return value;
  }
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) {
      throw new EarsError('INVALID_ARGUMENT', `Query numeric value must be finite (got ${String(value)})`);
    }
    return value;
  }
  if (value === undefined) {
    throw new EarsError('INVALID_ARGUMENT', 'Query value cannot be undefined');
  }
  if (typeof value === 'bigint') {
    throw new EarsError('INVALID_ARGUMENT', 'BigInt values are not supported in query filters or cursors');
  }
  if (typeof value === 'symbol' || typeof value === 'function') {
    throw new EarsError('INVALID_ARGUMENT', `Unsupported query value type '${typeof value}'`);
  }
  const realBytes = asRealUint8Array(value);
  if (realBytes !== null) {
    // Rust protocol `coerce_json_for_field` decodes `Bytes` fields from JSON `number[]` (0..=255).
    return Array.from(realBytes);
  }
  if (value instanceof Date) {
    let ms: number;
    try {
      ms = Date.prototype.getTime.call(value);
    } catch {
      throw new EarsError('INVALID_ARGUMENT', 'Invalid Date passed to query filter or cursor');
    }
    if (!Number.isFinite(ms)) {
      throw new EarsError('INVALID_ARGUMENT', 'Invalid Date passed to query filter or cursor');
    }
    // Rust protocol `coerce_json_for_field` accepts epoch milliseconds for `Timestamp` fields.
    return ms;
  }
  if (Array.isArray(value)) {
    if (depth >= MAX_QUERY_VALUE_DEPTH) {
      throw new EarsError(
        'INVALID_ARGUMENT',
        `Query value exceeds maximum nesting depth (${MAX_QUERY_VALUE_DEPTH})`,
      );
    }
    if (seen.has(value)) {
      throw new EarsError('INVALID_ARGUMENT', 'Cyclic reference in query value');
    }
    seen.add(value);
    try {
      const out: unknown[] = [];
      for (let i = 0; i < value.length; i++) {
        out.push(normalizeQueryValue(value[i], seen, depth + 1));
      }
      return out;
    } finally {
      seen.delete(value);
    }
  }
  if (isPlainRecord(value)) {
    if (depth >= MAX_QUERY_VALUE_DEPTH) {
      throw new EarsError(
        'INVALID_ARGUMENT',
        `Query value exceeds maximum nesting depth (${MAX_QUERY_VALUE_DEPTH})`,
      );
    }
    if (seen.has(value)) {
      throw new EarsError('INVALID_ARGUMENT', 'Cyclic reference in query value');
    }
    seen.add(value);
    try {
      const out: Record<string, unknown> = {};
      for (const key of Object.keys(value)) {
        if (key === '__proto__') {
          throw new EarsError('INVALID_ARGUMENT', "Reserved key '__proto__' is not allowed in query values");
        }
        if (hasLoneSurrogate(key)) {
          throw new EarsError('INVALID_ARGUMENT', 'Query object key contains an unpaired UTF-16 surrogate');
        }
        const v = value[key];
        if (v !== undefined) out[key] = normalizeQueryValue(v, seen, depth + 1);
      }
      return out;
    } finally {
      seen.delete(value);
    }
  }
  let ctorName: string = typeof value;
  try {
    if (typeof value === 'object' && value !== null && value.constructor && typeof value.constructor.name === 'string') {
      ctorName = value.constructor.name;
    }
  } catch {
    ctorName = 'object';
  }
  throw new EarsError('INVALID_ARGUMENT', `Unsupported non-plain query value of type '${ctorName}'`);
}

function compileFieldPredicate(field: string, valueOrPredicate: WhereInput): QueryFilter {
  if (valueOrPredicate === undefined) {
    throw new EarsError('INVALID_ARGUMENT', `Filter value for field '${field}' cannot be undefined`);
  }
  if (!isPlainRecord(valueOrPredicate)) {
    return { op: 'eq', field, value: normalizeQueryValue(valueOrPredicate) };
  }

  const keys = Object.keys(valueOrPredicate);
  if (keys.length === 0) {
    throw new EarsError(
      'INVALID_ARGUMENT',
      `Empty filter predicate for field '${field}' (use where({ op: 'eq', field: '${field}', value: {} }) to match an empty object)`,
    );
  }
  const unknownKeys = keys.filter((k) => !Object.prototype.hasOwnProperty.call(PREDICATE_KEYS, k));
  if (unknownKeys.length > 0) {
    throw new EarsError(
      'INVALID_ARGUMENT',
      `Unknown filter predicate key(s) for field '${field}': ${unknownKeys.join(', ')} (use { eq: obj } to match a plain object value)`,
    );
  }

  const pred = valueOrPredicate as FieldPredicate;
  if (pred.gt !== undefined && pred.gte !== undefined) {
    throw new EarsError(
      'INVALID_ARGUMENT',
      `Conflicting lower range bounds for field '${field}': specify either 'gt' or 'gte', not both`,
    );
  }
  if (pred.lt !== undefined && pred.lte !== undefined) {
    throw new EarsError(
      'INVALID_ARGUMENT',
      `Conflicting upper range bounds for field '${field}': specify either 'lt' or 'lte', not both`,
    );
  }

  const filters: QueryFilter[] = [];
  if (pred.eq !== undefined) {
    filters.push({ op: 'eq', field, value: normalizeQueryValue(pred.eq) });
  }
  if (pred.ne !== undefined) {
    filters.push({ op: 'ne', field, value: normalizeQueryValue(pred.ne) });
  }
  if (pred.in !== undefined) {
    if (!Array.isArray(pred.in)) {
      throw new EarsError('INVALID_ARGUMENT', `Filter 'in' predicate for field '${field}' must be an array`);
    }
    const values: unknown[] = [];
    for (let i = 0; i < pred.in.length; i++) {
      values.push(normalizeQueryValue(pred.in[i]));
    }
    filters.push({ op: 'in', field, values });
  }
  if (pred.gt !== undefined || pred.gte !== undefined || pred.lt !== undefined || pred.lte !== undefined) {
    const low: [unknown, boolean] | undefined =
      pred.gte !== undefined
        ? [normalizeQueryValue(pred.gte), true]
        : pred.gt !== undefined
          ? [normalizeQueryValue(pred.gt), false]
          : undefined;
    const high: [unknown, boolean] | undefined =
      pred.lte !== undefined
        ? [normalizeQueryValue(pred.lte), true]
        : pred.lt !== undefined
          ? [normalizeQueryValue(pred.lt), false]
          : undefined;
    filters.push({
      op: 'range',
      field,
      ...(low !== undefined && { low }),
      ...(high !== undefined && { high }),
    });
  }
  if (pred.startsWith !== undefined) {
    filters.push({ op: 'startsWith', field, prefix: pred.startsWith });
  }
  if (pred.contains !== undefined) {
    filters.push({ op: 'contains', field, needle: pred.contains });
  }
  if (pred.regex !== undefined) {
    filters.push({ op: 'regex', field, pattern: pred.regex });
  }
  if (pred.isNull !== undefined) {
    filters.push(
      pred.isNull
        ? { op: 'isNull', field }
        : { op: 'not', filter: { op: 'isNull', field } },
    );
  }
  if (pred.isPresent !== undefined) {
    filters.push(
      pred.isPresent
        ? { op: 'isPresent', field }
        : { op: 'not', filter: { op: 'isPresent', field } },
    );
  }
  if (filters.length === 0) {
    throw new EarsError('INVALID_ARGUMENT', `Empty filter predicate for field '${field}'`);
  }
  return filters.length === 1 ? filters[0]! : { op: 'and', filters };
}

function isGraphAnalyzeStep(step: QueryStep | undefined): boolean {
  return step !== undefined && Object.prototype.hasOwnProperty.call(GRAPH_ANALYZE_STEP_KINDS, step.kind);
}

function isGraphHitsStep(step: QueryStep | undefined): boolean {
  return step !== undefined && (step.kind === 'graphJaccard' || step.kind === 'graphAdamicAdar');
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
  /** Top-K Jaccard neighborhood similarity graph step (consume via `hits()` / `hitsAsync()`). */
  jaccard(seedId: string, topK: number, opts?: SimilarityOptions): QueryBuilder<EntityRow>;
  /** Top-K Adamic-Adar neighborhood similarity graph step (consume via `hits()` / `hitsAsync()`). */
  adamicAdar(seedId: string, topK: number, opts?: SimilarityOptions): QueryBuilder<EntityRow>;

  /** Orders results by a field (with optional ICU collation `locale`). */
  orderBy(field: string, dir?: SortDirection, opts?: OrderByOptions): QueryBuilder<TResult>;
  /** Orders results by computed score (default `'desc'`). */
  orderByScore(dir?: SortDirection): QueryBuilder<TResult>;
  /** Orders results by external ID (default `'asc'`). */
  orderById(dir?: SortDirection): QueryBuilder<TResult>;
  /** Keyset pagination cursor applied to the current ordering. */
  after(cursor: KeysetCursor): QueryBuilder<TResult>;
  /** Caps the number of returned results (must be a non-negative safe integer). */
  limit(n: number): QueryBuilder<TResult>;

  /**
   * Explicit single-snapshot JavaScript post-filter applied to materialized rows (`all`/`first`/`watch`).
   * Must be placed at the end of the pipeline (chaining further Rust steps or calling non-row terminals
   * like `count`/`ids`/`sum`/`minMax`/`groupBy`/`degrees` throws `INVALID_ARGUMENT`).
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
  /** Returns scored `{ id, score, row?, highlights? }` hits synchronously (also consumes `jaccard` / `adamicAdar`). */
  hits(opts?: HitsOptions): Hit[];
  /** Returns scored `{ id, score, row?, highlights? }` hits asynchronously (also consumes `jaccard` / `adamicAdar`). */
  hitsAsync(opts?: HitsOptions): Promise<Hit[]>;
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
  /** Groups matching entity IDs by `field` asynchronously. */
  groupByAsync(field: string): Promise<Array<[unknown, string[]]>>;
  /** Counts matching entities grouped by `field` synchronously. */
  groupCounts(field: string): Array<[unknown, number]>;
  /** Counts matching entities grouped by `field` asynchronously. */
  groupCountsAsync(field: string): Promise<Array<[unknown, number]>>;
  /** Computes per-entity edge degrees synchronously. */
  degrees(opts?: DegreesOptions): Array<[string, number]>;
  /** Computes per-entity edge degrees asynchronously. */
  degreesAsync(opts?: DegreesOptions): Promise<Array<[string, number]>>;
  /** Executes a community / component graph query (`leiden`, `scc`, `weakComponents`) synchronously. */
  graphGroups(): string[][];
  /** Executes a community / component graph query (`leiden`, `scc`, `weakComponents`) asynchronously. */
  graphGroupsAsync(): Promise<string[][]>;
  /** Executes a `topoSort` graph query synchronously. */
  graphTopo(): GraphTopoResult;
  /** Executes a `topoSort` graph query asynchronously. */
  graphTopoAsync(): Promise<GraphTopoResult>;
  /** Executes a `ppr` graph query synchronously. */
  graphPpr(): GraphPprResult;
  /** Executes a `ppr` graph query asynchronously. */
  graphPprAsync(): Promise<GraphPprResult>;

  /** Subscribes to live row updates driven by the engine's sans-I/O `WatchPump`. */
  watch(callback: (rows: TResult[]) => void, opts?: WatchRowsOptions): WatchHandle;
  /** Subscribes to live ID list updates. */
  watchIds(callback: (ids: string[]) => void, opts?: WatchOptions): WatchHandle;
  /** Subscribes to live scored hit updates (with optional projected fields and UTF-16 highlights). */
  watchHits(callback: (hits: Hit[]) => void, opts?: WatchHitsOptions): WatchHandle;
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
  readonly #implicitOrderSlice: boolean;
  readonly #observer: EarsOperationObserver | undefined;
  readonly #inUnit: () => boolean;

  constructor(
    db: EarsDatabase,
    pump: WatchPump,
    source: QuerySource,
    steps: readonly QueryStep[] = [],
    localFilters: ReadonlyArray<(row: TResult) => boolean> = [],
    implicitOrderSlice = false,
    observer?: EarsOperationObserver,
    inUnit: () => boolean = () => false,
  ) {
    this.#db = db;
    this.#pump = pump;
    this.#source = source;
    this.#steps = steps;
    this.#localFilters = localFilters;
    this.#implicitOrderSlice = implicitOrderSlice;
    this.#observer = observer;
    this.#inUnit = inUnit;
  }

  #assertNoLocalFilters(stepName: string): void {
    if (this.#localFilters.length > 0) {
      throw new EarsError(
        'INVALID_ARGUMENT',
        `Cannot chain '${stepName}()' after filterLocal() — place filterLocal() at the end of the pipeline`,
      );
    }
  }

  #withStep(step: QueryStep, implicitOrderSlice = false): QueryBuilderImpl<TResult> {
    this.#assertNoLocalFilters(step.kind);
    return new QueryBuilderImpl<TResult>(
      this.#db,
      this.#pump,
      this.#source,
      [...this.#steps, step],
      [],
      implicitOrderSlice,
      this.#observer,
      this.#inUnit,
    );
  }

  #withUntypedStep(step: QueryStep): QueryBuilderImpl<EntityRow> {
    this.#assertNoLocalFilters(step.kind);
    return new QueryBuilderImpl<EntityRow>(
      this.#db,
      this.#pump,
      this.#source,
      [...this.#steps, step],
      [],
      false,
      this.#observer,
      this.#inUnit,
    );
  }

  #withOrderStep(step: Extract<QueryStep, { kind: 'orderSlice' }>): QueryBuilderImpl<TResult> {
    this.#assertNoLocalFilters('orderBy');
    if (this.#implicitOrderSlice) {
      throw new EarsError(
        'INVALID_ARGUMENT',
        'Call orderBy() / orderByScore() / orderById() before limit() or after()',
      );
    }
    return this.#withStep(step, false);
  }

  #withUpdatedOrderSlice(
    patch: {
      readonly resetCursor?: boolean;
      readonly afterId?: string;
      readonly afterScore?: number;
      readonly afterValue?: unknown | null;
      readonly limit?: number;
    },
  ): QueryBuilderImpl<TResult> {
    this.#assertNoLocalFilters(patch.limit !== undefined ? 'limit' : 'after');
    const last = this.#steps[this.#steps.length - 1];
    if (last && last.kind === 'orderSlice') {
      const next: Extract<QueryStep, { kind: 'orderSlice' }> = {
        kind: 'orderSlice',
        by: last.by,
        dir: last.dir,
        ...(last.field !== undefined && { field: last.field }),
        ...(last.locale !== undefined && { locale: last.locale }),
        ...(!patch.resetCursor && last.afterId !== undefined && { afterId: last.afterId }),
        ...(!patch.resetCursor && last.afterScore !== undefined && { afterScore: last.afterScore }),
        ...(!patch.resetCursor && last.afterValue !== undefined && { afterValue: last.afterValue }),
        ...(last.limit !== undefined && { limit: last.limit }),
        ...(patch.afterId !== undefined && { afterId: patch.afterId }),
        ...(patch.afterScore !== undefined && { afterScore: patch.afterScore }),
        ...(patch.afterValue !== undefined && { afterValue: patch.afterValue }),
        ...(patch.limit !== undefined && { limit: patch.limit }),
      };
      return new QueryBuilderImpl<TResult>(
        this.#db,
        this.#pump,
        this.#source,
        [...this.#steps.slice(0, -1), next],
        [],
        this.#implicitOrderSlice,
        this.#observer,
        this.#inUnit,
      );
    }
    // Scan backwards to the most recent order-defining step (score, hops, shortestPath, or prior orderSlice)
    // so `search().hops().limit(n)` does not reverse hop order AND `orderBy(...).where(...).limit(n)`
    // preserves the explicit `orderBy(...)` ordering instead of clobbering it with `id asc`!
    let priorOrder: Extract<QueryStep, { kind: 'orderSlice' }> | undefined;
    let hasScoreSinceReset = false;
    for (let i = this.#steps.length - 1; i >= 0; i--) {
      const s = this.#steps[i];
      if (!s) continue;
      if (s.kind === 'score') {
        hasScoreSinceReset = true;
        break;
      }
      if (s.kind === 'orderSlice') {
        priorOrder = s;
        break;
      }
      if (s.kind === 'hops' || s.kind === 'shortestPath') {
        break;
      }
    }
    const created: Extract<QueryStep, { kind: 'orderSlice' }> = priorOrder
      ? {
          kind: 'orderSlice',
          by: priorOrder.by,
          dir: priorOrder.dir,
          ...(priorOrder.field !== undefined && { field: priorOrder.field }),
          ...(priorOrder.locale !== undefined && { locale: priorOrder.locale }),
          ...(patch.afterId !== undefined && { afterId: patch.afterId }),
          ...(patch.afterScore !== undefined && { afterScore: patch.afterScore }),
          ...(patch.afterValue !== undefined && { afterValue: patch.afterValue }),
          ...(patch.limit !== undefined && { limit: patch.limit }),
        }
      : {
          kind: 'orderSlice',
          by: hasScoreSinceReset ? 'score' : 'id',
          dir: hasScoreSinceReset ? 'desc' : 'asc',
          ...(patch.afterId !== undefined && { afterId: patch.afterId }),
          ...(patch.afterScore !== undefined && { afterScore: patch.afterScore }),
          ...(patch.afterValue !== undefined && { afterValue: patch.afterValue }),
          ...(patch.limit !== undefined && { limit: patch.limit }),
        };
    return this.#withStep(created, priorOrder === undefined);
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

  #existingZeroLimit(): boolean {
    const last = this.#steps[this.#steps.length - 1];
    return last !== undefined && last.kind === 'orderSlice' && last.limit === 0;
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
      ...(opts?.prefix !== undefined && { prefix: opts.prefix }),
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
    return this.#withUntypedStep({
      kind: 'hops',
      dir: opts.dir ?? 'out',
      minHops: opts.minHops ?? 1,
      maxHops: opts.maxHops,
      ...(opts.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
    });
  }

  shortestPath(targetId: string, opts: ShortestPathOptions): QueryBuilder<EntityRow> {
    return this.#withUntypedStep({
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
    return this.#withUntypedStep({
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
    return this.#withUntypedStep({
      kind: 'graphWeakComponents',
      ...(relKinds !== undefined && { relKinds: [...relKinds] }),
    });
  }

  scc(relKinds?: readonly string[]): QueryBuilder<EntityRow> {
    return this.#withUntypedStep({
      kind: 'graphScc',
      ...(relKinds !== undefined && { relKinds: [...relKinds] }),
    });
  }

  topoSort(relKinds?: readonly string[]): QueryBuilder<EntityRow> {
    return this.#withUntypedStep({
      kind: 'graphTopoSort',
      ...(relKinds !== undefined && { relKinds: [...relKinds] }),
    });
  }

  leiden(opts?: LeidenOptions): QueryBuilder<EntityRow> {
    return this.#withUntypedStep({
      kind: 'graphLeiden',
      resolution: opts?.resolution ?? 1.0,
      seed: opts?.seed ?? 42,
      iterations: opts?.iterations ?? 10,
      ...(opts?.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
      ...(opts?.weighted !== undefined && { weighted: opts.weighted }),
    });
  }

  jaccard(seedId: string, topK: number, opts?: SimilarityOptions): QueryBuilder<EntityRow> {
    return this.#withUntypedStep({
      kind: 'graphJaccard',
      dir: opts?.dir ?? 'both',
      seedId,
      topK,
      ...(opts?.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
    });
  }

  adamicAdar(seedId: string, topK: number, opts?: SimilarityOptions): QueryBuilder<EntityRow> {
    return this.#withUntypedStep({
      kind: 'graphAdamicAdar',
      dir: opts?.dir ?? 'both',
      seedId,
      topK,
      ...(opts?.relKinds !== undefined && { relKinds: [...opts.relKinds] }),
    });
  }

  orderBy(field: string, dir: SortDirection = 'asc', opts?: OrderByOptions): QueryBuilder<TResult> {
    return this.#withOrderStep({
      kind: 'orderSlice',
      by: 'field',
      field,
      dir,
      ...(opts?.locale !== undefined && { locale: opts.locale }),
    });
  }

  orderByScore(dir: SortDirection = 'desc'): QueryBuilder<TResult> {
    return this.#withOrderStep({ kind: 'orderSlice', by: 'score', dir });
  }

  orderById(dir: SortDirection = 'asc'): QueryBuilder<TResult> {
    return this.#withOrderStep({ kind: 'orderSlice', by: 'id', dir });
  }

  after(cursor: KeysetCursor): QueryBuilder<TResult> {
    if (typeof cursor.id !== 'string' || cursor.id.length === 0) {
      throw new EarsError('INVALID_ARGUMENT', 'Keyset cursor requires a non-empty string id');
    }
    if (cursor.score !== undefined && !Number.isFinite(cursor.score)) {
      throw new EarsError('INVALID_ARGUMENT', 'Keyset cursor score must be a finite number');
    }
    return this.#withUpdatedOrderSlice({
      resetCursor: true,
      afterId: cursor.id,
      ...(cursor.score !== undefined && { afterScore: cursor.score }),
      ...(cursor.value !== undefined && { afterValue: normalizeQueryValue(cursor.value) }),
    });
  }

  limit(n: number): QueryBuilder<TResult> {
    if (!Number.isSafeInteger(n) || n < 0) {
      throw new EarsError('INVALID_ARGUMENT', `limit() requires a non-negative safe integer, got ${String(n)}`);
    }
    return this.#withUpdatedOrderSlice({ limit: n });
  }

  filterLocal(predicate: (row: TResult) => boolean): QueryBuilder<TResult> {
    return new QueryBuilderImpl<TResult>(
      this.#db,
      this.#pump,
      this.#source,
      this.#steps,
      [...this.#localFilters, predicate],
      this.#implicitOrderSlice,
      this.#observer,
      this.#inUnit,
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
    const finish = this.#observer?.start({ kind: 'read', ops: 1, watch: false, inUnit: this.#inUnit() });
    let answer: QueryAnswer;
    try {
      answer = this.#db.query(this.toSpec(terminal, explain));
    } catch (error) {
      const earsErr = toEarsError(error);
      finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
      throw earsErr;
    }
    finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
    return answer;
  }

  async #runAsync(terminal?: QueryTerminal, explain?: boolean): Promise<QueryAnswer> {
    const finish = this.#observer?.start({ kind: 'read', ops: 1, watch: false, inUnit: false });
    let answer: QueryAnswer;
    try {
      answer = await this.#db.queryAsync(this.toSpec(terminal, explain));
    } catch (error) {
      const earsErr = toEarsError(error);
      finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
      throw earsErr;
    }
    finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
    return answer;
  }

  #hitsTerminal(opts?: HitsOptions): QueryTerminal | undefined {
    const last = this.#steps[this.#steps.length - 1];
    if (isGraphHitsStep(last) && opts?.fields === undefined && opts?.highlights !== true) {
      return undefined;
    }
    return {
      kind: 'hits',
      ...(opts?.fields !== undefined && { fields: [...opts.fields] }),
      ...(opts?.highlights !== undefined && { highlights: opts.highlights }),
    };
  }

  explain(terminal?: QueryTerminal): QueryAnswer {
    const last = this.#steps[this.#steps.length - 1];
    const effectiveTerminal = terminal ?? (isGraphAnalyzeStep(last) ? undefined : { kind: 'rows' });
    return this.#run(effectiveTerminal, true);
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
    const rows =
      this.#localFilters.length === 0 && !this.#existingZeroLimit()
        ? this.limit(1).all(fields)
        : this.all(fields);
    return rows[0] ?? null;
  }

  async firstAsync(fields?: readonly string[]): Promise<TResult | null> {
    const rows =
      this.#localFilters.length === 0 && !this.#existingZeroLimit()
        ? await this.limit(1).allAsync(fields)
        : await this.allAsync(fields);
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

  hits(opts?: HitsOptions): Hit[] {
    this.#forbidLocalFilter('hits');
    const ans = this.#run(this.#hitsTerminal(opts));
    if (ans.kind !== 'hits') {
      throw new EarsError('INTERNAL', `Expected 'hits' query answer, got '${ans.kind}'`);
    }
    return ans.hits;
  }

  async hitsAsync(opts?: HitsOptions): Promise<Hit[]> {
    this.#forbidLocalFilter('hitsAsync');
    const ans = await this.#runAsync(this.#hitsTerminal(opts));
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

  async groupByAsync(field: string): Promise<Array<[unknown, string[]]>> {
    this.#forbidLocalFilter('groupByAsync');
    const ans = await this.#runAsync({ kind: 'groupBy', field, countsOnly: false });
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

  async groupCountsAsync(field: string): Promise<Array<[unknown, number]>> {
    this.#forbidLocalFilter('groupCountsAsync');
    const ans = await this.#runAsync({ kind: 'groupBy', field, countsOnly: true });
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

  async degreesAsync(opts?: DegreesOptions): Promise<Array<[string, number]>> {
    this.#forbidLocalFilter('degreesAsync');
    const ans = await this.#runAsync({
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

  async graphGroupsAsync(): Promise<string[][]> {
    this.#forbidLocalFilter('graphGroupsAsync');
    const ans = await this.#runAsync();
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

  async graphTopoAsync(): Promise<GraphTopoResult> {
    this.#forbidLocalFilter('graphTopoAsync');
    const ans = await this.#runAsync();
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

  async graphPprAsync(): Promise<GraphPprResult> {
    this.#forbidLocalFilter('graphPprAsync');
    const ans = await this.#runAsync();
    if (ans.kind !== 'graphPpr') {
      throw new EarsError('INTERNAL', `Expected 'graphPpr' query answer, got '${ans.kind}'`);
    }
    return { scores: ans.scores, converged: ans.converged, iterations: ans.iterations };
  }

  watch(callback: (rows: TResult[]) => void, opts?: WatchRowsOptions): WatchHandle {
    const term: QueryTerminal = opts?.fields === undefined ? { kind: 'rows' } : { kind: 'rows', fields: [...opts.fields] };
    const spec = this.toSpec(term);
    const localFilters = this.#localFilters;
    const dedupPostFilter = localFilters.length > 0 && opts?.mode !== 'imperative';
    let lastFilteredJson: string | undefined;
    const computeFiltered = (answer: QueryAnswer): TResult[] => {
      if (answer.kind !== 'rows') {
        throw new EarsError('INTERNAL', `Expected 'rows' watch answer, got '${answer.kind}'`);
      }
      const typed = answer.rows as unknown as TResult[];
      return localFilters.length === 0 ? [...typed] : typed.filter((r) => localFilters.every((fn) => fn(r)));
    };
    return this.#pump.register(
      spec,
      (answer) => {
        const filtered = computeFiltered(answer);
        if (dedupPostFilter) {
          const serialized = JSON.stringify(filtered);
          if (serialized === lastFilteredJson) return;
          lastFilteredJson = serialized;
        }
        callback(filtered);
      },
      opts,
      dedupPostFilter
        ? (initialAnswer) => {
            lastFilteredJson = JSON.stringify(computeFiltered(initialAnswer));
          }
        : undefined,
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

  watchHits(callback: (hits: Hit[]) => void, opts?: WatchHitsOptions): WatchHandle {
    this.#forbidLocalFilter('watchHits');
    return this.#pump.register(
      this.toSpec(this.#hitsTerminal(opts)),
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
export function createQueryRoot(
  db: EarsDatabase,
  pump: WatchPump,
  observer?: EarsOperationObserver,
  inUnit: () => boolean = () => false,
): QueryRoot {
  return {
    of(typeName, opts) {
      return new QueryBuilderImpl(
        db,
        pump,
        {
          kind: 'type',
          typeName,
          ...(opts?.includeTrashed !== undefined && { includeTrashed: opts.includeTrashed }),
        },
        [],
        [],
        false,
        observer,
        inUnit,
      );
    },
    ofTypes(typeNames, opts) {
      return new QueryBuilderImpl(
        db,
        pump,
        {
          kind: 'types',
          typeNames: [...typeNames],
          ...(opts?.includeTrashed !== undefined && { includeTrashed: opts.includeTrashed }),
        },
        [],
        [],
        false,
        observer,
        inUnit,
      );
    },
    allEntities(opts) {
      return new QueryBuilderImpl(
        db,
        pump,
        {
          kind: 'allEntities',
          ...(opts?.includeTrashed !== undefined && { includeTrashed: opts.includeTrashed }),
        },
        [],
        [],
        false,
        observer,
        inUnit,
      );
    },
    ids(ids) {
      return new QueryBuilderImpl(db, pump, { kind: 'ids', ids: [...ids] }, [], [], false, observer, inUnit);
    },
    raw(spec) {
      const finish = observer?.start({ kind: 'read', ops: 1, watch: false, inUnit: inUnit() });
      let answer: QueryAnswer;
      try {
        answer = db.query(spec);
      } catch (error) {
        const earsErr = toEarsError(error);
        finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
        throw earsErr;
      }
      finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
      return answer;
    },
    async rawAsync(spec) {
      const finish = observer?.start({ kind: 'read', ops: 1, watch: false, inUnit: false });
      let answer: QueryAnswer;
      try {
        answer = await db.queryAsync(spec);
      } catch (error) {
        const earsErr = toEarsError(error);
        finish?.({ outcome: 'error', requestBytes: 0, responseBytes: 0, errorCode: earsErr.code });
        throw earsErr;
      }
      finish?.({ outcome: 'ok', requestBytes: 0, responseBytes: 0 });
      return answer;
    },
  };
}
