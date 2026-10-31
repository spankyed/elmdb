/**
 * EARS runtime types: what an engine is created with, and the query and
 * transaction builders.
 *
 * CHANGE CONTROL: these types are a specified contract, and editor completions and error messages
 * depend on their exact form. Don't change them to make one call site compile; fix the call site.
 * Read packages/apack-sdk/TYPED-EARS.md (the contract and the pre-change checklist) first.
 */
import type { EARS, EntityNameArg, EntityShapes, ShapeOf } from './entities.ts';
import type { RESERVED_ATTRIBUTES } from './errors.ts';
import type { EarsDatabase } from './database.ts';

/**
 * An id as `write` takes it: an entity's, plain or tagged with its type, and never a relation's (`EARS.RelationId`),
 * which the compiler refuses here since a relation is no entity
 */
export type WriteTarget<E extends string = string> = EARS.EntityId<E> & { readonly __relation?: never };

/** A relation row — its id, kind, ends and info, as `findRelations` returns one — with its ends typed */
export interface RelationEdge<K extends string = string, Source extends string = string, Target extends string = string> {
  id: EARS.RelationId;
  kind: K;
  source: EARS.EntityId<Source>;
  target: EARS.EntityId<Target>;
  info?: EARS.AttributeValue;
}

/** What an engine is created with (`createEarsEngine`) */
export interface EARSRuntimeDeps {
  /**
   * The elmdb database the engine reads and writes, opened by `@apackdev/ears/lmdb` (`openDatabase`). Its schema is
   * the engine's entity types, relation kinds and roles, and the rules their rows and edges are held to; the engine's
   * creator changes it with `admin.setSchema`
   */
  database: EarsDatabase;
  /**
   * Freezes every value a read gives back, as a test runtime does: code that changes one, expecting the database to
   * change, throws there rather than silently not persisting in the app. Off by default
   */
  freeze?: boolean;
  /**
   * Told when a query does something its data will make slow — a `where` filtering thousands of a type's rows for
   * want of an index — once per call site. Nothing by default; the app passes one in development
   */
  warn?: (message: string) => void;
}

/**
 * What a created row of an entity type is held to, which codegen (`apack pack generate-entries`) writes from the pack's
 * declarations: the required fields read off its shape, and the short code prefix its manifest declares
 */
export interface RowRules {
  /** The fields its row must hold: the shape's required ones. `create` refuses a row without one, and `delete` one */
  required: readonly string[];
  /**
   * The prefix `create` numbers the row's `shortCode` with, `<prefix>-<n>`: the one the manifest declares for the type
   * (its entry's `shortCode`), from which a pack's generated shapes type the field (`ShortCode<prefix>`). A number is never handed out twice
   * for a type and prefix, and a type with none holds no short code
   */
  shortCode?: string;
  /**
   * The fields a `where(field, value)` on the type reads an index for. The engine indexes `role` and `shortCode` on
   * every type; a `where` on any other field filters the type's rows, finding the same matches
   */
  indexes?: readonly string[];
  /** The fields its text index reads, each at its weight, and the language its words are stemmed in: what `search` matches */
  search?: SearchDeclaration;
  /** The fields its vector index embeds, and the model it embeds them with: what `near`, `like` and `hybrid` rank by */
  embed?: EmbedDeclaration;
  /** The fields its trigram index reads, strings or lists of strings: what `containing` and `matching` find */
  substrings?: readonly string[];
  /**
   * The type of each field the shape declares, as its TypeScript type maps to one of elmdb's (`FieldRule`): what the
   * database holds every write to, from every writer. A field the shape doesn't declare is still taken
   */
  fields?: Readonly<Record<string, FieldRule>>;
}

/**
 * A field's type as the database holds a write to it, read off a shape's property: text, a finite number, a boolean,
 * one of listed strings, an id of listed entity types, a list of one of these, or `any` for what maps to none of them
 */
export type FieldType =
  | { readonly type: 'string' | 'number' | 'boolean' | 'any' }
  | { readonly type: 'enum'; readonly values: readonly string[] }
  | { readonly type: 'ref'; readonly to: readonly string[] }
  | { readonly type: 'list'; readonly of: FieldType };

/** A field's type, and whether the row may lack it (`?`) and whether it may hold `null` (`| null`) */
export type FieldRule = FieldType & { readonly optional?: boolean; readonly nullable?: boolean };

/** A type's text index, as its manifest entry declares it (`search`) */
export interface SearchDeclaration {
  /** Each field the index reads, with its weight: a field of weight 3 counts each of its words three times */
  readonly fields: Readonly<Record<string, number>>;
  /** The language whose stems a word is matched by (`en`, `de`, …), when the index stems */
  readonly stem?: string;
}

/**
 * A type's vector index, as its manifest entry declares it (`embed`). A write that changes one of its fields leaves the
 * entity's chunks pending until an embedding of their text is stored (`embed`), which a later write's change refuses as
 * stale. Embeddings are compared by cosine distance
 */
export interface EmbedDeclaration {
  /** The fields whose text is embedded, in this order: strings, or lists of strings */
  readonly from: readonly string[];
  /** How the text is split, each part embedded on its own: whole (the default), at blank lines, or at Markdown headings */
  readonly chunk?: 'whole' | 'paragraph' | 'heading';
  /** The model the embeddings are made with: an embedding made with another is refused */
  readonly model: string;
  /** The numbers in each embedding, which the model decides */
  readonly dims: number;
}

/** Chunks of a type's vector index waiting for embeddings, and what to make them with */
export interface PendingEmbeddings {
  model: string;
  dims: number;
  /** How many chunks wait in all, those beyond the limit included */
  count: number;
  /** Up to the limit of them: each one's entity, its number in the entity, its text's hash and the text to embed */
  chunks: Array<{ id: EARS.EntityId; chunk: number; hash: string; text: string }>;
}

/** An embedding of one pending chunk, naming the chunk as `pendingEmbeddings` did */
export interface Embedding {
  id: EARS.EntityId;
  chunk: number;
  hash: string;
  model: string;
  vector: readonly number[];
}

/**
 * How `search` matches and ranks. `fields` names the index's fields a match may be in, and `recency` a field holding a
 * time, by which a score is halved for every `halfLife` milliseconds before `now` (the read's clock unless given)
 */
export interface SearchOptions<F extends string = string> {
  /** The query's last word matches the words it starts, as a search box does while it is typed in */
  prefix?: boolean;
  /** A word of five letters or more also matches one an edit away, nine or more two; on unless false */
  fuzzy?: boolean;
  /** Any of the query's clauses matching is enough, rather than every one */
  any?: boolean;
  fields?: readonly F[];
  recency?: { field: F; halfLife: number; now?: number };
}

/** Where a search's terms are in one field: each `[element, start, end]`, the element a list field's, in UTF-16 units */
export type SearchHighlights = Record<string, ReadonlyArray<readonly [element: number, start: number, end: number]>>;

/** What a search found in one entity */
export interface SearchHit<E extends string = string> {
  id: EARS.EntityId<E>;
  /** Its relevance: BM25 over the index's fields at their weights, decayed by recency when asked */
  score: number;
  /** The index's words it matched: the query's, and those its prefixes and edits reached */
  terms: string[];
  /** With `highlights`, where the matched words are in each field */
  highlights?: SearchHighlights;
}

/** Which of an entity's edges a walk follows (`pathTo`, `degrees`, `rank`): those out of it, those into it, or both */
export type EdgeDirection = 'out' | 'in' | 'both';

/** One stage of a query's plan as the database ran it: its start, a step or its terminal (`explain()`) */
export interface QueryStage {
  /** The stage's name: `types`, `ids` or `all` for the start, a step's (`where`, `orderBy`, …), the terminal's */
  stage: string;
  /** The value index it read, or `none: walks the type` for a lookup no built index covers */
  index?: string;
  /** What the stage before handed it, and what it read from the store itself */
  visited: number;
  /** What it handed on; for the terminal, what it answers */
  kept: number;
  /** Milliseconds spent in the stage's own work */
  ms: number;
  /** For a stage that built a graph, which `explain()` reaches only through a `rank`: its entities */
  nodes?: number;
  /** For a stage that built a graph: the edges between its entities */
  edges?: number;
  /** For a `rank`: the rounds it ran */
  iterations?: number;
  /** For a `rank`: whether its scores settled within the rounds it ran */
  converged?: boolean;
}

/** A query's ids, and how the database found them: every stage of its plan, the entities it read, and its time */
export interface QueryExplanation<E extends string = string> {
  ids: EARS.EntityId<E>[];
  /** Entities the plan read from the store, its index entries included */
  visited: number;
  /** Milliseconds the read took in the database */
  ms: number;
  stages: QueryStage[];
}

/**
 * A short code, `<prefix>-<n>`: how a pack's generated shapes type `shortCode` for an entity type whose manifest entry
 * declares the prefix, and how the SDK's own shapes type theirs. `create` numbers it, and nothing writes it after
 */
export type ShortCode<P extends string> = `${P}-${number}`;


// ─── QueryBuilder fluent interface ─────────────────────────────────────

/** `T` without being an inference site (TypeScript 5.4's NoInfer, for 5.3) */
type NoInferType<T> = [T][T extends unknown ? 0 : never];

/**
 * What a typed builder is checked against, as one parameter: a pack's `#generated/ears` declares its own
 * (`PackSchema`), so an error or a hover names the schema rather than spelling out four types
 */
export interface EarsSchema {
  /** The entity shapes reads and writes are typed against */
  shapes: EntityShapes;
  /** The entity names accepted as arguments */
  names: string;
  /** The relation kinds links, walks and edge reads take */
  relKinds: string;
  /** The roles grants and `withRole` take */
  roles: string;
}

/** No shapes and any name, kind or role: the untyped operations (`@apackdev/ears`'s own, and the SDK's `untyped*`) */
export interface UntypedSchema extends EarsSchema {
  shapes: {};
  names: string;
  relKinds: string;
  roles: string;
}

/**
 * The query builder, typed by `D` (`UntypedSchema` unchecked, a pack's `PackSchema` from its `#generated/ears`).
 *
 * Field parameters stay `keyof ShapeOf<D['shapes'], E> & string` and picked rows `Pick<ShapeOf<D['shapes'], E>, K>`:
 * that form is what gives editors field completions and lists the valid fields in a typo's error.
 * Wrapping them in conditional or mapped types keeps everything compiling and passing type tests
 * while breaking completions (TYPED-EARS.md, Incidents). For a union shape (Node rows) only shared
 * fields are accepted; a query over member-specific or runtime fields uses the untyped host `query`.
 */
export interface QueryBuilder<E extends string = string, D extends EarsSchema = UntypedSchema> {
  ofType<T extends string>(t: EntityNameArg<D['names'], T>): QueryBuilder<T, D>;
  /** The entities among `ids` */
  within(ids: readonly EARS.EntityId[]): QueryBuilder<E, D>;
  /**
   * The entities whose `k` is `v` (`===`) or a list holding it, or for which `v`, a function, answers true given the
   * whole value. A primitive `v` is answered by the value index. `v` is required: `has` and `lacks` ask about presence.
   */
  where<K extends keyof ShapeOf<D['shapes'], E> & string>(k: K, v: WhereValue<D['shapes'], E, K>): QueryBuilder<E, D>;
  /** The entities with a value of `k` */
  has<K extends keyof ShapeOf<D['shapes'], E> & string>(k: K): QueryBuilder<E, D>;
  /** The entities with no value of `k` */
  lacks<K extends keyof ShapeOf<D['shapes'], E> & string>(k: K): QueryBuilder<E, D>;
  withRole(r: D['roles']): QueryBuilder<E, D>;
  /**
   * The entities with a `kind` edge to `other` — or, without it, to any entity this query could reach (a filter: the
   * set stays these entities)
   */
  linkedTo(kind: D['relKinds'], other?: EARS.EntityId): QueryBuilder<E, D>;
  /** The entities `other` has a `kind` edge to — or, without it, that any entity this query could reach has one to */
  linkedFrom(kind: D['relKinds'], other?: EARS.EntityId): QueryBuilder<E, D>;
  /** The entities `linkedTo(kind, other?)` leaves out */
  unlinkedTo(kind: D['relKinds'], other?: EARS.EntityId): QueryBuilder<E, D>;
  /** The entities `linkedFrom(kind, other?)` leaves out: a tree's roots are `unlinkedFrom(parentKind)` */
  unlinkedFrom(kind: D['relKinds'], other?: EARS.EntityId): QueryBuilder<E, D>;
  /** The entities with an edge of any kind, either way, to `other` (a filter) */
  linkedWith(other: EARS.EntityId): QueryBuilder<E, D>;
  /**
   * Walks the entities' outgoing edges of the kinds to the entities at their other end — unlike `linkedTo`, which
   * filters. The result holds `type` entities, not `E`; with `type` omitted the target is unknown.
   */
  out<T extends string>(
    kinds: D['relKinds'] | readonly D['relKinds'][],
    type: EntityNameArg<D['names'], T> | readonly EntityNameArg<D['names'], T>[] | undefined,
  ): QueryBuilder<T, D>;
  // Target absent — the resulting entity type is unknown. A target known only at runtime
  // (typed string) takes the overload above and yields QueryBuilder<string>.
  out(kinds: D['relKinds'] | readonly D['relKinds'][], type?: undefined): QueryBuilder<string, D>;
  /** Walks the entities' incoming edges of the kinds to the entities at their other end */
  in<T extends string>(
    kinds: D['relKinds'] | readonly D['relKinds'][],
    type: EntityNameArg<D['names'], T> | readonly EntityNameArg<D['names'], T>[] | undefined,
  ): QueryBuilder<T, D>;
  in(kinds: D['relKinds'] | readonly D['relKinds'][], type?: undefined): QueryBuilder<string, D>;
  /** Walks the entities' edges of the kinds both ways, out and in, to the entities at their other end, each once */
  both<T extends string>(
    kinds: D['relKinds'] | readonly D['relKinds'][],
    type: EntityNameArg<D['names'], T> | readonly EntityNameArg<D['names'], T>[] | undefined,
  ): QueryBuilder<T, D>;
  both(kinds: D['relKinds'] | readonly D['relKinds'][], type?: undefined): QueryBuilder<string, D>;
  /**
   * Every entity the set reaches through outgoing edges of the kinds, any number deep or at most `depth`: a subtree,
   * nearest first, each once, never an entity of the set itself. `type` keeps the reached entities of the types.
   */
  descendants<T extends string>(
    kinds: D['relKinds'] | readonly D['relKinds'][],
    type: EntityNameArg<D['names'], T> | readonly EntityNameArg<D['names'], T>[] | undefined,
    options?: { depth?: number },
  ): QueryBuilder<T, D>;
  descendants(kinds: D['relKinds'] | readonly D['relKinds'][], type?: undefined, options?: { depth?: number }): QueryBuilder<string, D>;
  /**
   * Every entity that reaches the set through edges of the kinds, any number deep or at most `depth`: in a tree, the
   * parent first and the root last, each once, never an entity of the set itself. `type` keeps the reached entities
   * of the types.
   */
  ancestors<T extends string>(
    kinds: D['relKinds'] | readonly D['relKinds'][],
    type: EntityNameArg<D['names'], T> | readonly EntityNameArg<D['names'], T>[] | undefined,
    options?: { depth?: number },
  ): QueryBuilder<T, D>;
  ancestors(kinds: D['relKinds'] | readonly D['relKinds'][], type?: undefined, options?: { depth?: number }): QueryBuilder<string, D>;
  /**
   * A shortest path through edges of the kinds from an entity of the set to `to`: its entities in order, both ends
   * included, or none when there is no path (of at most `depth` edges, when given). Edges are followed `out` unless
   * `direction` says `in` or `both`; of paths as short, the first found. Its entities may be of any type.
   */
  pathTo(to: EARS.EntityId, kinds: D['relKinds'] | readonly D['relKinds'][], options?: { direction?: EdgeDirection; depth?: number }): QueryBuilder<string, D>;
  /**
   * Each outgoing edge of the kinds whose target this query could reach (of `type`, when given): its relation row,
   * `findRelations`' shape, one per edge
   */
  outEdges<K extends D['relKinds'], T extends string = string>(kinds: K | readonly K[], type?: EntityNameArg<D['names'], T> | EntityNameArg<D['names'], T>[]): Array<RelationEdge<K, E, NoInferType<T>>>;
  /** Each incoming edge of the kinds whose source this query could reach: its relation row */
  inEdges<K extends D['relKinds'], T extends string = string>(kinds: K | readonly K[], type?: EntityNameArg<D['names'], T> | EntityNameArg<D['names'], T>[]): Array<RelationEdge<K, NoInferType<T>, E>>;
  /**
   * Each entity's number of edges of the kinds, in query order, `0` included: those `outEdges` (or with `direction`,
   * `inEdges`, or both) would answer, so only edges whose other end this query could reach, of `type` when given
   */
  degrees<T extends string = string>(
    kinds: D['relKinds'] | readonly D['relKinds'][],
    type?: EntityNameArg<D['names'], T> | readonly EntityNameArg<D['names'], T>[],
    options?: { direction?: EdgeDirection },
  ): Map<EARS.EntityId<E>, number>;
  /**
   * The query's entities by how much they matter from `seeds`, highest first, ties in query order: personalized
   * PageRank over the edges of the kinds between two of them, followed `out` unless `direction` says `in` or `both`. A
   * walker follows one of an entity's edges, or jumps back to a seed; an entity's score is the share of its time spent
   * there. With no `seeds` every entity is one, which ranks what is central overall. A seed the query doesn't reach is
   * ignored, and none it reaches answers nothing. `hits()` answers each score, and `limit(n)` keeps the `n` highest.
   */
  rank(kinds: D['relKinds'] | readonly D['relKinds'][], options?: { direction?: EdgeDirection; seeds?: readonly EARS.EntityId[] }): QueryBuilder<E, D>;
  /**
   * The query's weak components: its entities in groups joined by edges of the kinds between two of them, whichever way
   * each points: each group's ids in query order, the largest group first, of two as large the one whose first entity
   * comes first
   */
  weakComponents(kinds: D['relKinds'] | readonly D['relKinds'][]): EARS.EntityId<E>[][];
  pick<A extends readonly (keyof ShapeOf<D['shapes'], E> & string)[]>(
    fields: A,
  ): ({ id: EARS.EntityId<E> } & Pick<ShapeOf<D['shapes'], E>, A[number]>)[];
  /** The first entity's fields picked (a field it lacks left out), or undefined when the query reaches none */
  pickOne<A extends readonly (keyof ShapeOf<D['shapes'], E> & string)[]>(
    f: A,
  ): ({ id: EARS.EntityId<E> } & Pick<ShapeOf<D['shapes'], E>, A[number]>) | undefined;
  /** The first entity's whole row (`pickAll`'s, `getRow`'s), or undefined when the query reaches none: `query.ids(id).pickOne()` is the live row with an id */
  pickOne(): ShapeOf<D['shapes'], E> | undefined;
  pickAll(): ShapeOf<D['shapes'], E>[];
  orderBy(field: keyof ShapeOf<D['shapes'], E> & string, dir?: 'asc' | 'desc'): QueryBuilder<E, D>;
  reverse(): QueryBuilder<E, D>;
  limit(n: number): QueryBuilder<E, D>;
  distinct(field?: keyof ShapeOf<D['shapes'], E> & string): QueryBuilder<E, D>;
  /** The ids reached, by their value of `field`, each group in query order; `query.ids(ids)` queries a group on */
  groupBy(field: keyof ShapeOf<D['shapes'], E> & string): Map<unknown, EARS.EntityId<E>[]>;
  /**
   * The entities the type's text index (its manifest entry's `search`) matches to `query`, most relevant first: words,
   * every one of which matches; `"a phrase"`; `-word`, which no match holds; `field:word`; `a OR b`. After other
   * steps it searches among the entities they reached
   */
  search(query: string, options?: SearchOptions<keyof ShapeOf<D['shapes'], E> & string>): QueryBuilder<E, D>;
  /**
   * The entities whose text, in a field of the type's trigram index (its manifest entry's `substrings`), holds `text`
   * as it is written, or with `caseInsensitive` in any case: an error message, an id, a path. Unranked: the query keeps
   * its order, and after other steps it filters the entities they reached
   */
  containing(text: string, options?: { caseInsensitive?: boolean }): QueryBuilder<E, D>;
  /**
   * The entities whose text, in a field of the type's trigram index, matches `pattern`, in Rust's `regex` syntax
   * (`(?i)` ignores case), which has no look-around or backreferences: a pattern using them is refused
   * (`PATTERN_INVALID`). Unranked, as `containing` is
   */
  matching(pattern: string): QueryBuilder<E, D>;
  /**
   * The `k` entities (10 unless given) whose embeddings in the type's vector index (its manifest entry's `embed`) are
   * nearest `vector`, nearest first, each scored `1 / (1 + distance)` by its nearest chunk; after other steps, among the
   * entities they reached. An entity whose chunks are pending is not found until they are embedded
   */
  near(vector: readonly number[], options?: { k?: number }): QueryBuilder<E, D>;
  /** `near` the embedding the entity with `id` holds, itself excepted: more like it */
  like(id: EARS.EntityId, options?: { k?: number }): QueryBuilder<E, D>;
  /**
   * The text index's matches to `query` and the vector index's nearest to `vector`, merged by reciprocal rank fusion:
   * an entity both find ranks above one either finds alone. The `k` best (10 unless given)
   */
  hybrid(query: string, vector: readonly number[], options?: SearchOptions<keyof ShapeOf<D['shapes'], E> & string> & { k?: number }): QueryBuilder<E, D>;
  /** The entities after `id` in the order so far: the page after one that ended at `id` */
  after(id: EARS.EntityId): QueryBuilder<E, D>;
  /**
   * What the query's `search` (or `near`, `like`, `hybrid` or `rank`, whose hits hold no words) found in each entity it reaches: its score and matched words, with `highlights` where
   * they are, and with `fields` its row's fields picked (`pick`'s)
   */
  hits<A extends readonly (keyof ShapeOf<D['shapes'], E> & string)[]>(
    options: { fields: A; highlights?: boolean },
  ): Array<SearchHit<E> & { row: { id: EARS.EntityId<E> } & Pick<ShapeOf<D['shapes'], E>, A[number]> }>;
  hits(options?: { highlights?: boolean }): SearchHit<E>[];
  ids(): EARS.EntityId<E>[];
  /**
   * The query's ids, with how the database found them: each stage of its plan — the index it read, the entities it
   * visited and kept, its time. Refused (`NOT_EXPLAINABLE`) for a query with a `where` given a function, which runs
   * outside the database's plan
   */
  explain(): QueryExplanation<E>;
  count(): number;
  /** The first entity's id, or undefined when the query reaches none */
  first(): EARS.EntityId<E> | undefined;
  /** The last entity's id, or undefined when the query reaches none */
  last(): EARS.EntityId<E> | undefined;
  exists(): boolean;
  map<T>(fn: (id: EARS.EntityId<E>) => T): T[];
  forEach(fn: (id: EARS.EntityId<E>) => void): QueryBuilder<E, D>;
  reduce<T>(fn: (acc: T, id: EARS.EntityId<E>) => T, init: T): T;
}

// ─── WriteBuilder fluent interface ─────────────────────────────────────

/**
 * How `link` makes an edge. What a kind's edges are held to — no cycle through it, at most one edge between two
 * entities — is the kind's declaration (`apack.json` `data.relations`), not a link's
 */
export interface LinkOptions {
  /** Stored on the edge, and part of what makes it the same edge as one already there */
  info?: unknown;
}

/** What `create` is given besides the type */
/**
 * The engine's own fields a created row may carry beside its fields, none of which a field can be named: its id and
 * its times, for an import keeping the ones it had
 */
export interface RowIdentity {
  /** The new entity's id, in place of a generated one; one already in use is refused */
  id?: EARS.EntityId;
  /** When it was created, in place of now: an import keeping an original time */
  createdAt?: number;
  /** When it last changed, in place of its creation time: an import keeping an original time */
  updatedAt?: number;
}

/**
 * The short code a created row may bring as identity, in place of the next number — an import keeping the code it
 * had: for a declared shape, its own `shortCode` type (`ShortCode<prefix>`), so another type's code fails to compile,
 * and no such key when the shape has no short code; any string for an undeclared type. The engine holds every one to
 * the type's `<prefix>-<n>` and to no other entity holding it at runtime
 */
export type ShortCodeIdentity<S extends EntityShapes, E extends string> =
  [E] extends [keyof S]
    ? (ShapeOf<S, E> extends { shortCode?: infer C } ? { shortCode?: C } : {})
    : { shortCode?: string };

/** What `write` is given besides the entity */
export interface WriteOptions {
  /**
   * Whether the builder's attribute writes stamp `updatedAt` (they do by default). `false` is for a write that is no
   * edit of the entity — when it was last viewed, a position kept for later.
   */
  touch?: boolean;
}

/**
 * The fields a write may name: when `S` declares `E`'s shape, its declared fields but the engine's own
 * (`RESERVED_ATTRIBUTES`), and any name otherwise (an id with no entity type, or an untyped write)
 */
export type WritableField<S extends EntityShapes, E extends string> =
  [E] extends [keyof S] ? Exclude<keyof ShapeOf<S, E> & string, (typeof RESERVED_ATTRIBUTES)[number]> : string;

/**
 * The fields a write may remove: a declared shape's optional writable fields, since removing a required one would
 * leave a row its shape misdescribes (`set` replaces it); any name otherwise
 */
export type OptionalField<S extends EntityShapes, E extends string> =
  [E] extends [keyof S]
    ? Extract<{ [K in keyof ShapeOf<S, E>]-?: {} extends Pick<ShapeOf<S, E>, K> ? K : never }[keyof ShapeOf<S, E>], WritableField<S, E>>
    : string;

/** The fields a declared shape requires: its writable fields that aren't optional, which `create` takes up front */
export type RequiredField<S extends EntityShapes, E extends string> =
  [E] extends [keyof S] ? Exclude<WritableField<S, E>, OptionalField<S, E>> : never;

/**
 * The row `create` writes: for a declared shape, every required field and any optional one, so a row read back has
 * what its shape says it has; any fields otherwise (an undeclared type, or one known only at runtime). The engine's
 * own fields, the short code among them, are not fields of a row: an import brings them as its identity
 * (`RowIdentity`, `ShortCodeIdentity`)
 */
export type CreateFields<S extends EntityShapes, E extends string> =
  [E] extends [keyof S]
    ? { [K in keyof ShapeOf<S, E> as K extends RequiredField<S, E> ? K : never]: ShapeOf<S, E>[K] }
      & { [K in keyof ShapeOf<S, E> as K extends OptionalField<S, E> ? K : never]?: ShapeOf<S, E>[K] }
    : Record<string, unknown>;

/** What `where(k, v)` matches field `K` against: a value, one element of a list field, or a predicate */
export type WhereValue<S extends EntityShapes, E extends string, K extends keyof ShapeOf<S, E>> =
  | ShapeOf<S, E>[K]
  | (NonNullable<ShapeOf<S, E>[K]> extends readonly (infer U)[] ? U : never)
  | ((value: ShapeOf<S, E>[K]) => boolean);

/** What a write may store under field `K`: the declared field's type when `S` declares `E`'s shape, anything otherwise */
export type FieldValue<S extends EntityShapes, E extends string, K extends string> =
  [E] extends [keyof S] ? (K extends keyof ShapeOf<S, E> ? ShapeOf<S, E>[K] : unknown) : unknown;

/** An element of a declared list field (`T[]`), and anything for a field that isn't declared as one */
export type ListElement<T> = [T] extends [readonly (infer U)[]] ? U : unknown;

/** Fields for a batch write: a declared shape's writable fields with their declared types, any fields otherwise */
export type FieldValues<S extends EntityShapes, E extends string> =
  [E] extends [keyof S]
    ? { [K in keyof ShapeOf<S, E> as K extends (typeof RESERVED_ATTRIBUTES)[number] ? never : K]?: ShapeOf<S, E>[K] }
    : Record<string, unknown>;

/**
 * The writes to one entity, from `create`, `upsert` or `write`. `E` and `D` type them: started from a
 * declared entity type or an id tagged with one, a write names only the shape's declared fields and stores
 * their declared types; from a plain id (or from `@apackdev/ears`) every write is unchecked. The attributes the engine owns (`id`,
 * `entityType`, `shortCode`, `createdAt`, `updatedAt`, `role`, `trashedAt`) are refused by every attribute write, and
 * each attribute write stamps `updatedAt` once per builder unless it was started with `{ touch: false }`.
 *
 * A relation is no entity, so a relation's id (`EARS.RelationId`) is not written through this builder:
 * `updateRelation(id, { source?, target?, info? })` changes one and `removeRelation(id)` removes it.
 */
export interface WriteBuilder<E extends string = string, D extends EarsSchema = UntypedSchema> {
  /**
   * Replaces the attribute's value with `v` (a list is one array value). `null` is a value; `undefined` is refused,
   * since `delete(k)` is what removes an attribute
   */
  set<K extends WritableField<D['shapes'], E>>(k: K, v: FieldValue<D['shapes'], E, K>): WriteBuilder<E, D>;
  /** `set` for each field, leaving one whose value is `undefined` as it is; refuses them all, writing none, when one is the engine's */
  setMany(attrs: FieldValues<D['shapes'], E>): WriteBuilder<E, D>;
  /** Appends `v` to the attribute's list, starting one when it has none; refuses a value that isn't a list */
  append<K extends WritableField<D['shapes'], E>>(k: K, v: ListElement<FieldValue<D['shapes'], E, K>>): WriteBuilder<E, D>;
  /** Merges `v` into the attribute's object, or sets it when either isn't a plain object */
  merge<K extends WritableField<D['shapes'], E>>(k: K, v: Partial<FieldValue<D['shapes'], E, K>>): WriteBuilder<E, D>;
  /** Removes the field; a declared shape's required fields can't be removed, only replaced with `set` */
  delete(k: OptionalField<D['shapes'], E>): WriteBuilder<E, D>;
  /** Removes the list's element at `index`; refuses a value that isn't a list */
  removeAt(k: WritableField<D['shapes'], E>, index: number): WriteBuilder<E, D>;
  /** Removes the list's first element equal to `v` (for an object, the first holding each of `v`'s fields) */
  removeValue<K extends WritableField<D['shapes'], E>>(k: K, v: ListElement<FieldValue<D['shapes'], E, K>>): WriteBuilder<E, D>;
  /**
   * Links this entity to `target`. An edge of the kind to the target with the same `info` (or any, when
   * none is given) is the same edge, and linking it again changes nothing; of a kind declared `unique`, any edge to
   * the target is, and linking it again with `info` replaces the edge's. A kind declared `acyclic` refuses an edge
   * that would close a cycle through it (`CYCLE`).
   */
  link(kind: D['relKinds'], target: EARS.EntityId, options?: LinkOptions): WriteBuilder<E, D>;
  /** Removes this entity's edges of the kind, to `target` only when given */
  unlink(kind: D['relKinds'], target?: EARS.EntityId): WriteBuilder<E, D>;
  /** Removes every edge from this entity */
  unlinkAll(): WriteBuilder<E, D>;
  grant(role: D['roles']): WriteBuilder<E, D>;
  revoke(role: D['roles']): WriteBuilder<E, D>;
  /** Grants the role here and revokes it from every other entity in `among` */
  grantExclusive(role: D['roles'], among: readonly EARS.EntityId[]): WriteBuilder<E, D>;
  /**
   * Puts the entity in the trash at `at` (now by default), marking it `trashedAt`: it stays stored, with its
   * edges, and queries reach it only through `query.withTrashed` and `query.trashed`. Trashing it again keeps
   * the first time.
   */
  trash(at?: number): WriteBuilder<E, D>;
  /** Takes the entity out of the trash */
  restore(): WriteBuilder<E, D>;
  /** Deletes the entity and every edge to or from it */
  destroy(): void;
  id(): EARS.EntityId<E>;
  /** The entity's whole row as the builder's writes left it (`getRow`'s): what a `create` reads back */
  row(): ShapeOf<D['shapes'], E>;
}
