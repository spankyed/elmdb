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
  onDropAttr(kind: string, entityId: string, idx: number, entireArray?: unknown[]): void;
  /** An attribute's values after a write, whole: the sink stores them in place of what it had */
  onPutAttrArray(kind: string, entityId: string, values: unknown[]): void;
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
  onPutAttrArray() {},
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
  /** Whether a name is an entity type (`tx('Note')` creates one) rather than an id */
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
 * fields are accepted; a query over member-specific or runtime fields uses the untyped host `qx`.
 */
export interface QueryBuilder<E extends string = string, S extends EntityShapes = {}, N extends string = string> {
  ofType<T extends string>(t: EntityNameArg<N, T>): QueryBuilder<T, S, N>;
  inIds(sub: readonly EARS.EntityId[]): QueryBuilder<E, S, N>;
  /**
   * The entities with a value of `k` equal (`===`) to `v`, or for which `v`, a function, answers true; an
   * attribute's values are tested one by one. `v` is required: `has` and `lacks` ask about presence.
   */
  where<K extends keyof ShapeOf<S, E> & string>(k: K, v: ShapeOf<S, E>[K] | ((value: ShapeOf<S, E>[K]) => boolean)): QueryBuilder<E, S, N>;
  /** The entities with a value of `k` */
  has<K extends keyof ShapeOf<S, E> & string>(k: K): QueryBuilder<E, S, N>;
  /** The entities with no value of `k` */
  lacks<K extends keyof ShapeOf<S, E> & string>(k: K): QueryBuilder<E, S, N>;
  withRole(r: string): QueryBuilder<E, S, N>;
  relatedTo(target: EARS.EntityId): QueryBuilder<E, S, N>;
  related(kind: string, other: EARS.EntityId, asSrc?: boolean): QueryBuilder<E, S, N>;
  /**
   * Navigates to the entities related to the current set — unlike `relatedTo`
   * and `related`, which filter it. The result therefore holds `tgtType`
   * entities, not `E`; with `tgtType` omitted the target is unknown.
   */
  linksTo<T extends string>(
    relKinds: string | readonly string[],
    tgtType: EntityNameArg<N, T> | readonly EntityNameArg<N, T>[] | undefined,
    asSrc?: boolean,
  ): QueryBuilder<T, S, N>;
  // Target absent — the resulting entity type is unknown. A target known only at runtime
  // (typed string) takes the overload above and yields QueryBuilder<string>.
  linksTo(
    relKinds: string | readonly string[],
    tgtType?: undefined,
    asSrc?: boolean,
  ): QueryBuilder<string, S, N>;
  links<K extends string, T extends string = string>(relKinds: K | readonly K[], tgtType?: EntityNameArg<N, T> | EntityNameArg<N, T>[], asSrc?: boolean): Array<{ relation: K; id: EARS.EntityId<NoInferType<T>> }>;
  edgeIds(kinds?: string | readonly string[], asSrc?: boolean): EARS.EntityId[];
  pick<A extends readonly (keyof ShapeOf<S, E> & string)[]>(
    fields: A,
  ): ({ id: EARS.EntityId<E> } & Pick<ShapeOf<S, E>, A[number]>)[];
  pickOne<A extends readonly (keyof ShapeOf<S, E> & string)[]>(
    f: A,
  ): ({ id: EARS.EntityId<E> } & Pick<ShapeOf<S, E>, A[number]>) | null;
  pickAll(): ShapeOf<S, E>[];
  /**
   * Fields here describe the relation's TARGET entity, not `E`, so they are
   * checked against `tgtType`. With `tgtType` omitted the target is unknown and
   * the field list is unconstrained.
   *
   * Two overloads because the implementation only tags rows with `relation`
   * when more than one relation kind is requested.
   */
  linksPick<K extends string, T extends string, A extends readonly (keyof ShapeOf<S, T> & string)[]>(
    relKinds: readonly [K, K, ...K[]],
    fields: A,
    tgtType?: EntityNameArg<N, T> | EntityNameArg<N, T>[],
  ): ({ id: EARS.EntityId<NoInferType<T>>; relation: K } & Pick<ShapeOf<S, T>, A[number]>)[];
  linksPick<K extends string, T extends string, A extends readonly (keyof ShapeOf<S, T> & string)[]>(
    relKinds: K | readonly [K],
    fields: A,
    tgtType?: EntityNameArg<N, T> | EntityNameArg<N, T>[],
  ): ({ id: EARS.EntityId<NoInferType<T>> } & Pick<ShapeOf<S, T>, A[number]>)[];
  orderBy(field: keyof ShapeOf<S, E> & string, dir?: 'asc' | 'desc'): QueryBuilder<E, S, N>;
  reverse(): QueryBuilder<E, S, N>;
  limit(n: number): QueryBuilder<E, S, N>;
  distinct(field?: keyof ShapeOf<S, E> & string): QueryBuilder<E, S, N>;
  groupBy(field: keyof ShapeOf<S, E> & string): Map<unknown, QueryBuilder<E, S, N>>;
  ids(): EARS.EntityId<E>[];
  id(): EARS.EntityId<E> | null;
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
}

/**
 * What a write may store under field `K`: the declared field's type when `S` declares `E`'s shape
 * and the field, anything otherwise (an undeclared field, or an id with no entity type).
 */
export type FieldValue<S extends EntityShapes, E extends string, K extends string> =
  [E] extends [keyof S] ? (K extends keyof ShapeOf<S, E> ? ShapeOf<S, E>[K] : unknown) : unknown;

/** Fields for a batch write: declared fields take their declared types, other fields anything */
export type FieldValues<S extends EntityShapes, E extends string> =
  [E] extends [keyof S] ? { [K in keyof ShapeOf<S, E>]?: ShapeOf<S, E>[K] } & Record<string, unknown> : Record<string, unknown>;

/**
 * The writes to one entity, from `create`, `upsert` or `write`. `E` and `S` type them: started from a
 * declared entity type or an id tagged with one, the values of declared fields are checked; from a plain
 * id (or from `@apackdev/ears`) every write is unchecked. The attributes the engine owns (`id`,
 * `entityType`, `createdAt`, `role`, `relationDetails`) are refused by every attribute write.
 */
export interface WriteBuilder<E extends string = string, S extends EntityShapes = {}> {
  /** Replaces the attribute's values with `v` */
  set<K extends string>(k: K, v: FieldValue<S, E, K>): WriteBuilder<E, S>;
  /** `set` for each field; refuses them all, writing none, when one is the engine's */
  setMany(attrs: FieldValues<S, E>): WriteBuilder<E, S>;
  /** Appends `v` to the attribute's values */
  add<K extends string>(k: K, v: FieldValue<S, E, K>): WriteBuilder<E, S>;
  /** Merges `v` into the object at `index` (the first by default), or sets it there */
  merge<K extends string>(k: K, v: Partial<FieldValue<S, E, K>>, index?: number): WriteBuilder<E, S>;
  /** Removes the attribute, every value of it */
  delete(k: string): WriteBuilder<E, S>;
  /** Removes the value at `index` */
  deleteAt(k: string, index: number): WriteBuilder<E, S>;
  /** Removes the first value equal to `v` (for an object, the first holding each of `v`'s fields) */
  deleteValue<K extends string>(k: K, v: FieldValue<S, E, K>): WriteBuilder<E, S>;
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
   * Deletes the entity and every edge to or from it. `keepStored` takes it out of the engine and leaves what
   * persistence stored of it, as clearing run records from memory does.
   */
  destroy(options?: { keepStored?: boolean }): void;
  id(): EARS.EntityId<E>;
}
