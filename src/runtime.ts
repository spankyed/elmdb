/**
 * EARS runtime types: the persistence port, what an engine is created with, and the query and
 * transaction builders.
 *
 * CHANGE CONTROL: these types are a specified contract, and editor completions and error messages
 * depend on their exact form. Don't change them to make one call site compile; fix the call site.
 * Read packages/apack-sdk/TYPED-EARS.md (the contract and the pre-change checklist) first.
 */
import type { EARS, EntityNameArg, EntityShapes, ShapeOf } from './entities.ts';

// ─── PersistenceSink interface ─────────────────────────────────────────

export interface PersistenceSink {
  /** An entity was created, as `entityType`: the one call that gives it a row */
  onCreateEntity(entityId: string, entityType: string): void;
  onDestroyEntity(entityId: string): void;
  /** An attribute was removed */
  onDropAttr(kind: string, entityId: string): void;
  /** An attribute's value after a write, whole (a list is one array value): the sink stores it in place of what it had */
  onPutAttr(kind: string, entityId: string, value: unknown): void;
  onAddRelation(relId: string, kind: string, src: string, tgt: string, info: unknown): void;
  onUpdateRelation(relId: string, patch: { src?: string; tgt?: string; info?: unknown }): void;
  /**
   * A relation (a link between two entities) was removed.
   * - `destroyed` is set when the link was removed only because one of its two entities is being deleted:
   *   it's the id of that entity. The sink may then keep the stored link (see makeShardedPersistence).
   * - `destroyed` is undefined when the link itself was removed (an unlink): the sink deletes it.
   */
  onRemoveRelation(relId: string, destroyed?: string): void;
  /**
   * A committed unit (`transact`) begins: the calls up to `onUnitEnd` are its writes, and the sink stores them
   * together or not at all. Calls outside a unit are each their own.
   */
  onUnitStart(): void;
  onUnitEnd(): void;
  close?(): void;
  getErrorStats?(): PersistenceErrorStats;
}

/** The writes a sink failed to store so far, and the last failure */
export interface PersistenceErrorStats {
  errorCount: number;
  lastError: unknown;
}

/** A sink that drops every write: an engine's persistence unless it's given one */
export const noopSink: PersistenceSink = {
  onCreateEntity() {},
  onDestroyEntity() {},
  onDropAttr() {},
  onPutAttr() {},
  onAddRelation() {},
  onUpdateRelation() {},
  onRemoveRelation() {},
  onUnitStart() {},
  onUnitEnd() {},
};

/** What an engine is created with (`createEarsEngine`) */
export interface EARSRuntimeDeps {
  /** Receives every write; nothing by default */
  persistence?: PersistenceSink;
  /** Whether a name is an entity type (`create('Note')` makes one) rather than an id */
  isEntityType: (v: string) => boolean;
}


// ─── QueryBuilder fluent interface ─────────────────────────────────────

/** `T` without being an inference site (TypeScript 5.4's NoInfer, for 5.3) */
type NoInferType<T> = [T][T extends unknown ? 0 : never];

/**
 * `S` is the entity shape map reads are typed against and `N` the entity names accepted as
 * arguments: `{}` and `string` (unchecked) from `@apackdev/ears`, the pack's `PackShapes` and
 * `EntityName` from its `#generated/ears`.
 *
 * Field parameters stay `keyof ShapeOf<S, E> & string` and picked rows `Pick<ShapeOf<S, E>, K>`:
 * that form is what gives editors field completions and lists the valid fields in a typo's error.
 * Wrapping them in conditional or mapped types keeps everything compiling and passing type tests
 * while breaking completions (TYPED-EARS.md, Incidents). For a union shape (Node rows) only shared
 * fields are accepted; a query over member-specific or runtime fields uses the untyped host `query`.
 */
export interface QueryBuilder<E extends string = string, S extends EntityShapes = {}, N extends string = string> {
  ofType<T extends string>(t: EntityNameArg<N, T>): QueryBuilder<T, S, N>;
  /** The entities among `ids` */
  within(ids: readonly EARS.EntityId[]): QueryBuilder<E, S, N>;
  /**
   * The entities whose `k` is `v` (`===`) or a list holding it, or for which `v`, a function, answers true given the
   * whole value. A primitive `v` is answered by the value index. `v` is required: `has` and `lacks` ask about presence.
   */
  where<K extends keyof ShapeOf<S, E> & string>(
    k: K,
    v: ShapeOf<S, E>[K] | (NonNullable<ShapeOf<S, E>[K]> extends readonly (infer U)[] ? U : never) | ((value: ShapeOf<S, E>[K]) => boolean),
  ): QueryBuilder<E, S, N>;
  /** The entities with a value of `k` */
  has<K extends keyof ShapeOf<S, E> & string>(k: K): QueryBuilder<E, S, N>;
  /** The entities with no value of `k` */
  lacks<K extends keyof ShapeOf<S, E> & string>(k: K): QueryBuilder<E, S, N>;
  withRole(r: string): QueryBuilder<E, S, N>;
  /** The entities with a `kind` edge to `other` (a filter: the set stays these entities) */
  linkedTo(kind: string, other: EARS.EntityId): QueryBuilder<E, S, N>;
  /** The entities `other` has a `kind` edge to (a filter) */
  linkedFrom(kind: string, other: EARS.EntityId): QueryBuilder<E, S, N>;
  /** The entities with an edge of any kind, either way, to `other` (a filter) */
  linkedWith(other: EARS.EntityId): QueryBuilder<E, S, N>;
  /**
   * Walks the entities' outgoing edges of the kinds to the entities at their other end — unlike `linkedTo`, which
   * filters. The result holds `type` entities, not `E`; with `type` omitted the target is unknown.
   */
  out<T extends string>(
    kinds: string | readonly string[],
    type: EntityNameArg<N, T> | readonly EntityNameArg<N, T>[] | undefined,
  ): QueryBuilder<T, S, N>;
  // Target absent — the resulting entity type is unknown. A target known only at runtime
  // (typed string) takes the overload above and yields QueryBuilder<string>.
  out(kinds: string | readonly string[], type?: undefined): QueryBuilder<string, S, N>;
  /** Walks the entities' incoming edges of the kinds to the entities at their other end */
  in<T extends string>(
    kinds: string | readonly string[],
    type: EntityNameArg<N, T> | readonly EntityNameArg<N, T>[] | undefined,
  ): QueryBuilder<T, S, N>;
  in(kinds: string | readonly string[], type?: undefined): QueryBuilder<string, S, N>;
  /** Each outgoing edge of the kinds: its kind and the entity at its other end, one row per edge */
  outEdges<K extends string, T extends string = string>(kinds: K | readonly K[], type?: EntityNameArg<N, T> | EntityNameArg<N, T>[]): Array<{ kind: K; id: EARS.EntityId<NoInferType<T>> }>;
  /** Each incoming edge of the kinds: its kind and the entity at its other end */
  inEdges<K extends string, T extends string = string>(kinds: K | readonly K[], type?: EntityNameArg<N, T> | EntityNameArg<N, T>[]): Array<{ kind: K; id: EARS.EntityId<NoInferType<T>> }>;
  pick<A extends readonly (keyof ShapeOf<S, E> & string)[]>(
    fields: A,
  ): ({ id: EARS.EntityId<E> } & Pick<ShapeOf<S, E>, A[number]>)[];
  pickOne<A extends readonly (keyof ShapeOf<S, E> & string)[]>(
    f: A,
  ): ({ id: EARS.EntityId<E> } & Pick<ShapeOf<S, E>, A[number]>) | null;
  pickAll(): ShapeOf<S, E>[];
  orderBy(field: keyof ShapeOf<S, E> & string, dir?: 'asc' | 'desc'): QueryBuilder<E, S, N>;
  reverse(): QueryBuilder<E, S, N>;
  limit(n: number): QueryBuilder<E, S, N>;
  distinct(field?: keyof ShapeOf<S, E> & string): QueryBuilder<E, S, N>;
  groupBy(field: keyof ShapeOf<S, E> & string): Map<unknown, QueryBuilder<E, S, N>>;
  ids(): EARS.EntityId<E>[];
  count(): number;
  first(): EARS.EntityId<E> | null;
  last(): EARS.EntityId<E> | null;
  exists(): boolean;
  map<T>(fn: (id: EARS.EntityId<E>) => T): T[];
  forEach(fn: (id: EARS.EntityId<E>) => void): QueryBuilder<E, S, N>;
  reduce<T>(fn: (acc: T, id: EARS.EntityId<E>) => T, init: T): T;
}

// ─── WriteBuilder fluent interface ─────────────────────────────────────

/** How `link` makes an edge */
export interface LinkOptions {
  /** Stored on the edge, and part of what makes it the same edge as one already there */
  info?: unknown;
  /** Removes this entity's edges of the kind to the target first, so the one linked is the only one */
  replace?: boolean;
  /** Refuses the edge when it would close a cycle through these kinds */
  acyclicIn?: readonly EARS.RelKind[];
  /** Links both ways */
  symmetric?: boolean;
}

/** What `create` is given besides the type */
export interface CreateOptions {
  /** The new entity's id, in place of a generated one; one already in use is refused */
  id?: EARS.EntityId;
  /** When it was created, in place of now: an import keeping an original time */
  createdAt?: number;
  /** When it last changed, in place of its creation time: an import keeping an original time */
  updatedAt?: number;
}

/** What `write` and `upsert` are given besides the entity */
export interface WriteOptions {
  /**
   * Whether the builder's attribute writes stamp `updatedAt` (they do by default). `false` is for a write that is no
   * edit of the entity — when it was last viewed, a position kept for later.
   */
  touch?: boolean;
}

/**
 * What a write may store under field `K`: the declared field's type when `S` declares `E`'s shape
 * and the field, anything otherwise (an undeclared field, or an id with no entity type).
 */
export type FieldValue<S extends EntityShapes, E extends string, K extends string> =
  [E] extends [keyof S] ? (K extends keyof ShapeOf<S, E> ? ShapeOf<S, E>[K] : unknown) : unknown;

/** An element of a declared list field (`T[]`), and anything for a field that isn't declared as one */
export type ListElement<T> = [T] extends [readonly (infer U)[]] ? U : unknown;

/** Fields for a batch write: declared fields take their declared types, other fields anything */
export type FieldValues<S extends EntityShapes, E extends string> =
  [E] extends [keyof S] ? { [K in keyof ShapeOf<S, E>]?: ShapeOf<S, E>[K] } & Record<string, unknown> : Record<string, unknown>;

/**
 * The writes to one entity, from `create`, `upsert` or `write`. `E` and `S` type them: started from a
 * declared entity type or an id tagged with one, the values of declared fields are checked; from a plain
 * id (or from `@apackdev/ears`) every write is unchecked. The attributes the engine owns (`id`,
 * `entityType`, `createdAt`, `updatedAt`, `role`, `relationDetails`, `trashedAt`) are refused by every attribute write, and
 * each attribute write stamps `updatedAt` once per builder unless it was started with `{ touch: false }`.
 */
export interface WriteBuilder<E extends string = string, S extends EntityShapes = {}> {
  /**
   * Replaces the attribute's value with `v` (a list is one array value). `null` is a value; `undefined` is refused,
   * since `delete(k)` is what removes an attribute
   */
  set<K extends string>(k: K, v: FieldValue<S, E, K>): WriteBuilder<E, S>;
  /** `set` for each field, leaving one whose value is `undefined` as it is; refuses them all, writing none, when one is the engine's */
  setMany(attrs: FieldValues<S, E>): WriteBuilder<E, S>;
  /** Appends `v` to the attribute's list, starting one when it has none; refuses a value that isn't a list */
  add<K extends string>(k: K, v: ListElement<FieldValue<S, E, K>>): WriteBuilder<E, S>;
  /** Merges `v` into the attribute's object, or sets it when either isn't a plain object */
  merge<K extends string>(k: K, v: Partial<FieldValue<S, E, K>>): WriteBuilder<E, S>;
  /** Removes the attribute */
  delete(k: string): WriteBuilder<E, S>;
  /** Removes the list's element at `index`; refuses a value that isn't a list */
  deleteAt(k: string, index: number): WriteBuilder<E, S>;
  /** Removes the list's first element equal to `v` (for an object, the first holding each of `v`'s fields) */
  deleteValue<K extends string>(k: K, v: ListElement<FieldValue<S, E, K>>): WriteBuilder<E, S>;
  /**
   * Links this entity to `target`. An edge of the kind to the target with the same `info` (or any, when
   * none is given) is the same edge, and linking it again changes nothing.
   */
  link(kind: string, target: EARS.EntityId, options?: LinkOptions): WriteBuilder<E, S>;
  /** Removes this entity's edges of the kind, to `target` only when given */
  unlink(kind: string, target?: EARS.EntityId): WriteBuilder<E, S>;
  /** Removes every edge from this entity */
  unlinkAll(): WriteBuilder<E, S>;
  grant(role: string): WriteBuilder<E, S>;
  revoke(role: string): WriteBuilder<E, S>;
  /** Grants the role here and revokes it from every other entity in `among` */
  grantExclusive(role: string, among: readonly EARS.EntityId[]): WriteBuilder<E, S>;
  /**
   * Puts the entity in the trash at `at` (now by default), marking it `trashedAt`: it stays stored, with its
   * edges, and queries reach it only through `query.withTrashed` and `query.trashed`. Trashing it again keeps
   * the first time.
   */
  trash(at?: number): WriteBuilder<E, S>;
  /** Takes the entity out of the trash */
  restore(): WriteBuilder<E, S>;
  /**
   * Deletes the entity and every edge to or from it. `keepStored` takes it out of the engine and leaves what
   * persistence stored of it, as clearing run records from memory does.
   */
  destroy(options?: { keepStored?: boolean }): void;
  id(): EARS.EntityId<E>;
}
