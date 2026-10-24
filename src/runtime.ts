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
import type { OrderedKV } from './kv.ts';

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
  /** The store the engine reads and writes: a new one in memory by default (`createMemoryKV`) */
  kv?: OrderedKV;
  /** Whether a name is an entity type (`create('Note')` makes one) rather than an id */
  isEntityType: (v: string) => boolean;
  /** Whether a name is a relation kind: `link`, and every read or walk naming a kind, refuse one that isn't */
  isRelationKind: (v: string) => boolean;
  /** Whether a name is a role: `grant`, `revoke`, `grantExclusive` and `withRole` refuse one that isn't */
  isRole: (v: string) => boolean;
  /** What a created row of a type is held to; nothing by default — an engine given no shapes knows of no rule */
  rowRules?: (type: string) => RowRules | undefined;
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
  /**
   * Every entity the set reaches through outgoing edges of the kinds, any number deep: a subtree, nearest first,
   * each once, never an entity of the set itself. `type` keeps the reached entities of the types.
   */
  descendants<T extends string>(
    kinds: D['relKinds'] | readonly D['relKinds'][],
    type: EntityNameArg<D['names'], T> | readonly EntityNameArg<D['names'], T>[] | undefined,
  ): QueryBuilder<T, D>;
  descendants(kinds: D['relKinds'] | readonly D['relKinds'][], type?: undefined): QueryBuilder<string, D>;
  /**
   * Every entity that reaches the set through edges of the kinds, any number deep: in a tree, the parent first
   * and the root last, each once, never an entity of the set itself. `type` keeps the reached entities of the types.
   */
  ancestors<T extends string>(
    kinds: D['relKinds'] | readonly D['relKinds'][],
    type: EntityNameArg<D['names'], T> | readonly EntityNameArg<D['names'], T>[] | undefined,
  ): QueryBuilder<T, D>;
  ancestors(kinds: D['relKinds'] | readonly D['relKinds'][], type?: undefined): QueryBuilder<string, D>;
  /**
   * Each outgoing edge of the kinds whose target this query could reach (of `type`, when given): its relation row,
   * `findRelations`' shape, one per edge
   */
  outEdges<K extends D['relKinds'], T extends string = string>(kinds: K | readonly K[], type?: EntityNameArg<D['names'], T> | EntityNameArg<D['names'], T>[]): Array<RelationEdge<K, E, NoInferType<T>>>;
  /** Each incoming edge of the kinds whose source this query could reach: its relation row */
  inEdges<K extends D['relKinds'], T extends string = string>(kinds: K | readonly K[], type?: EntityNameArg<D['names'], T> | EntityNameArg<D['names'], T>[]): Array<RelationEdge<K, NoInferType<T>, E>>;
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
  ids(): EARS.EntityId<E>[];
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
   * none is given) is the same edge, and linking it again changes nothing.
   */
  link(kind: D['relKinds'], target: EARS.EntityId, options?: LinkOptions<D['relKinds']>): WriteBuilder<E, D>;
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
