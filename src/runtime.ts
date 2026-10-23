/**
 * EARS runtime types: the persistence port, what an engine is created with, and the query and
 * transaction builders.
 *
 * CHANGE CONTROL: these types are a specified contract, and editor completions and error messages
 * depend on their exact form. Don't change them to make one call site compile; fix the call site.
 * Read packages/apack-sdk/TYPED-EARS.md (the contract and the pre-change checklist) first.
 */
import type { EARS, EntityNameArg, EntityShapes, ShapeOf } from './entities.ts';
import type { RESERVED_ATTRIBUTES } from './errors.ts';

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
  /** Whether a name is a relation kind: `link`, and every read or walk naming a kind, refuse one that isn't */
  isRelationKind: (v: string) => boolean;
  /** Whether a name is a role: `grant`, `revoke`, `grantExclusive` and `withRole` refuse one that isn't */
  isRole: (v: string) => boolean;
  /**
   * The fields an entity type's row must hold: `create` refuses a row without one, and `delete` one of them. None by
   * default — an engine given no shapes knows of no required field
   */
  requiredFields?: (type: string) => readonly string[];
}


// ─── QueryBuilder fluent interface ─────────────────────────────────────

/** `T` without being an inference site (TypeScript 5.4's NoInfer, for 5.3) */
type NoInferType<T> = [T][T extends unknown ? 0 : never];

/**
 * `S` is the entity shape map reads are typed against, `N` the entity names accepted as arguments, `R` the
 * relation kinds and `O` the roles: `{}` and `string` (unchecked) from `@apackdev/ears`, the pack's `PackShapes`,
 * `EntityName`, `EARS.RelKind` and `EARS.RoleKind` from its `#generated/ears`.
 *
 * Field parameters stay `keyof ShapeOf<S, E> & string` and picked rows `Pick<ShapeOf<S, E>, K>`:
 * that form is what gives editors field completions and lists the valid fields in a typo's error.
 * Wrapping them in conditional or mapped types keeps everything compiling and passing type tests
 * while breaking completions (TYPED-EARS.md, Incidents). For a union shape (Node rows) only shared
 * fields are accepted; a query over member-specific or runtime fields uses the untyped host `query`.
 */
export interface QueryBuilder<E extends string = string, S extends EntityShapes = {}, N extends string = string, R extends string = string, O extends string = string> {
  ofType<T extends string>(t: EntityNameArg<N, T>): QueryBuilder<T, S, N, R, O>;
  /** The entities among `ids` */
  within(ids: readonly EARS.EntityId[]): QueryBuilder<E, S, N, R, O>;
  /**
   * The entities whose `k` is `v` (`===`) or a list holding it, or for which `v`, a function, answers true given the
   * whole value. A primitive `v` is answered by the value index. `v` is required: `has` and `lacks` ask about presence.
   */
  where<K extends keyof ShapeOf<S, E> & string>(k: K, v: WhereValue<S, E, K>): QueryBuilder<E, S, N, R, O>;
  /** The entities with a value of `k` */
  has<K extends keyof ShapeOf<S, E> & string>(k: K): QueryBuilder<E, S, N, R, O>;
  /** The entities with no value of `k` */
  lacks<K extends keyof ShapeOf<S, E> & string>(k: K): QueryBuilder<E, S, N, R, O>;
  withRole(r: O): QueryBuilder<E, S, N, R, O>;
  /** The entities with a `kind` edge to `other` (a filter: the set stays these entities) */
  linkedTo(kind: R, other: EARS.EntityId): QueryBuilder<E, S, N, R, O>;
  /** The entities `other` has a `kind` edge to (a filter) */
  linkedFrom(kind: R, other: EARS.EntityId): QueryBuilder<E, S, N, R, O>;
  /** The entities with an edge of any kind, either way, to `other` (a filter) */
  linkedWith(other: EARS.EntityId): QueryBuilder<E, S, N, R, O>;
  /**
   * Walks the entities' outgoing edges of the kinds to the entities at their other end — unlike `linkedTo`, which
   * filters. The result holds `type` entities, not `E`; with `type` omitted the target is unknown.
   */
  out<T extends string>(
    kinds: R | readonly R[],
    type: EntityNameArg<N, T> | readonly EntityNameArg<N, T>[] | undefined,
  ): QueryBuilder<T, S, N, R, O>;
  // Target absent — the resulting entity type is unknown. A target known only at runtime
  // (typed string) takes the overload above and yields QueryBuilder<string>.
  out(kinds: R | readonly R[], type?: undefined): QueryBuilder<string, S, N, R, O>;
  /** Walks the entities' incoming edges of the kinds to the entities at their other end */
  in<T extends string>(
    kinds: R | readonly R[],
    type: EntityNameArg<N, T> | readonly EntityNameArg<N, T>[] | undefined,
  ): QueryBuilder<T, S, N, R, O>;
  in(kinds: R | readonly R[], type?: undefined): QueryBuilder<string, S, N, R, O>;
  /** Each outgoing edge of the kinds: its kind and the entity at its other end, one row per edge */
  outEdges<K extends R, T extends string = string>(kinds: K | readonly K[], type?: EntityNameArg<N, T> | EntityNameArg<N, T>[]): Array<{ kind: K; id: EARS.EntityId<NoInferType<T>> }>;
  /** Each incoming edge of the kinds: its kind and the entity at its other end */
  inEdges<K extends R, T extends string = string>(kinds: K | readonly K[], type?: EntityNameArg<N, T> | EntityNameArg<N, T>[]): Array<{ kind: K; id: EARS.EntityId<NoInferType<T>> }>;
  pick<A extends readonly (keyof ShapeOf<S, E> & string)[]>(
    fields: A,
  ): ({ id: EARS.EntityId<E> } & Pick<ShapeOf<S, E>, A[number]>)[];
  pickOne<A extends readonly (keyof ShapeOf<S, E> & string)[]>(
    f: A,
  ): ({ id: EARS.EntityId<E> } & Pick<ShapeOf<S, E>, A[number]>) | null;
  pickAll(): ShapeOf<S, E>[];
  orderBy(field: keyof ShapeOf<S, E> & string, dir?: 'asc' | 'desc'): QueryBuilder<E, S, N, R, O>;
  reverse(): QueryBuilder<E, S, N, R, O>;
  limit(n: number): QueryBuilder<E, S, N, R, O>;
  distinct(field?: keyof ShapeOf<S, E> & string): QueryBuilder<E, S, N, R, O>;
  groupBy(field: keyof ShapeOf<S, E> & string): Map<unknown, QueryBuilder<E, S, N, R, O>>;
  ids(): EARS.EntityId<E>[];
  count(): number;
  first(): EARS.EntityId<E> | null;
  last(): EARS.EntityId<E> | null;
  exists(): boolean;
  map<T>(fn: (id: EARS.EntityId<E>) => T): T[];
  forEach(fn: (id: EARS.EntityId<E>) => void): QueryBuilder<E, S, N, R, O>;
  reduce<T>(fn: (acc: T, id: EARS.EntityId<E>) => T, init: T): T;
}

// ─── WriteBuilder fluent interface ─────────────────────────────────────

/** How `link` makes an edge */
export interface LinkOptions<R extends string = string> {
  /** Stored on the edge, and part of what makes it the same edge as one already there */
  info?: unknown;
  /** Removes this entity's edges of the kind to the target first, so the one linked is the only one */
  replace?: boolean;
  /** Refuses the edge when it would close a cycle through these kinds */
  acyclicIn?: readonly R[];
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
 * what its shape says it has; any fields otherwise (an undeclared type, or one known only at runtime)
 */
export type CreateFields<S extends EntityShapes, E extends string> =
  [E] extends [keyof S]
    ? { [K in keyof ShapeOf<S, E> as K extends RequiredField<S, E> ? K : never]: ShapeOf<S, E>[K] }
      & { [K in keyof ShapeOf<S, E> as K extends OptionalField<S, E> ? K : never]?: ShapeOf<S, E>[K] }
    : Record<string, unknown>;

/** What `where(k, v)` and `findWhere` match field `K` against: a value, one element of a list field, or a predicate */
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
 * The writes to one entity, from `create`, `upsert` or `write`. `E` and `S` type them: started from a
 * declared entity type or an id tagged with one, a write names only the shape's declared fields and stores
 * their declared types; from a plain id (or from `@apackdev/ears`) every write is unchecked. The attributes the engine owns (`id`,
 * `entityType`, `createdAt`, `updatedAt`, `role`, `relationDetails`, `trashedAt`) are refused by every attribute write, and
 * each attribute write stamps `updatedAt` once per builder unless it was started with `{ touch: false }`.
 */
export interface WriteBuilder<E extends string = string, S extends EntityShapes = {}, R extends string = string, O extends string = string> {
  /**
   * Replaces the attribute's value with `v` (a list is one array value). `null` is a value; `undefined` is refused,
   * since `delete(k)` is what removes an attribute
   */
  set<K extends WritableField<S, E>>(k: K, v: FieldValue<S, E, K>): WriteBuilder<E, S, R, O>;
  /** `set` for each field, leaving one whose value is `undefined` as it is; refuses them all, writing none, when one is the engine's */
  setMany(attrs: FieldValues<S, E>): WriteBuilder<E, S, R, O>;
  /** Appends `v` to the attribute's list, starting one when it has none; refuses a value that isn't a list */
  add<K extends WritableField<S, E>>(k: K, v: ListElement<FieldValue<S, E, K>>): WriteBuilder<E, S, R, O>;
  /** Merges `v` into the attribute's object, or sets it when either isn't a plain object */
  merge<K extends WritableField<S, E>>(k: K, v: Partial<FieldValue<S, E, K>>): WriteBuilder<E, S, R, O>;
  /** Removes the attribute; a declared shape's required fields can't be removed, only replaced with `set` */
  delete(k: OptionalField<S, E>): WriteBuilder<E, S, R, O>;
  /** Removes the list's element at `index`; refuses a value that isn't a list */
  deleteAt(k: WritableField<S, E>, index: number): WriteBuilder<E, S, R, O>;
  /** Removes the list's first element equal to `v` (for an object, the first holding each of `v`'s fields) */
  deleteValue<K extends WritableField<S, E>>(k: K, v: ListElement<FieldValue<S, E, K>>): WriteBuilder<E, S, R, O>;
  /**
   * Links this entity to `target`. An edge of the kind to the target with the same `info` (or any, when
   * none is given) is the same edge, and linking it again changes nothing.
   */
  link(kind: R, target: EARS.EntityId, options?: LinkOptions<R>): WriteBuilder<E, S, R, O>;
  /** Removes this entity's edges of the kind, to `target` only when given */
  unlink(kind: R, target?: EARS.EntityId): WriteBuilder<E, S, R, O>;
  /** Removes every edge from this entity */
  unlinkAll(): WriteBuilder<E, S, R, O>;
  grant(role: O): WriteBuilder<E, S, R, O>;
  revoke(role: O): WriteBuilder<E, S, R, O>;
  /** Grants the role here and revokes it from every other entity in `among` */
  grantExclusive(role: O, among: readonly EARS.EntityId[]): WriteBuilder<E, S, R, O>;
  /**
   * Puts the entity in the trash at `at` (now by default), marking it `trashedAt`: it stays stored, with its
   * edges, and queries reach it only through `query.withTrashed` and `query.trashed`. Trashing it again keeps
   * the first time.
   */
  trash(at?: number): WriteBuilder<E, S, R, O>;
  /** Takes the entity out of the trash */
  restore(): WriteBuilder<E, S, R, O>;
  /**
   * Deletes the entity and every edge to or from it. `keepStored` takes it out of the engine and leaves what
   * persistence stored of it, as clearing run records from memory does. A relation's id (from `findRelations`)
   * removes that one relation, which is how one edge of several between the same ends goes.
   */
  destroy(options?: { keepStored?: boolean }): void;
  id(): EARS.EntityId<E>;
}
