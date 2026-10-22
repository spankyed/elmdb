/**
 * EARS query helpers typed against an entity shape map. This is the only pack-facing way
 * to get them: `apack pack generate-entries` writes `#generated/ears` with
 * `defineEars<PackShapes>()` (the pack's entities plus its dependencies'). There is no
 * global registry, so each pack's typing depends only on what it declares.
 *
 * CHANGE CONTROL: these types are a specified contract, and editor completions and error messages
 * depend on their exact form. Don't change them to make one call site compile; fix the call site.
 * Read packages/apack-sdk/TYPED-EARS.md (the contract and the pre-change checklist) first.
 * Overload order here is part of the contract: a signature that accepts more than the one before
 * it lets a call that should fail (a typo'd name or field) fall through to it.
 */
import type { EARS, EntityNameArg, EntityShapes, ShapeOf } from './entities.ts';
import type { CreateOptions, QueryBuilder, WriteBuilder } from './runtime.ts';
import { createEntity } from './attribute-storage.ts';
import type { CreatedEntityFields } from './transaction-helpers.ts';
import type { EarsQuery } from './engine.ts';
import { installedEngine } from './installed.ts';

/** An entity name argument: a declared name, or a name only known at runtime (see EntityNameArg) */
type Name<N extends string, E extends string> = EntityNameArg<N, E>;

/**
 * The query entry points: `qx(type)` from a declared entity type (its name checked, its rows typed), `qx.ids`
 * from ids (typed by the entity an id is tagged with), `qx.all` from every entity. A name and an id are
 * different arguments to different functions, so an id passed where a type is meant fails to compile.
 */
export interface TypedQx<S extends EntityShapes, N extends string = string> {
  <E extends string>(type: Name<N, E>): QueryBuilder<E, S, N>;
  <E extends string>(types: readonly Name<N, E>[]): QueryBuilder<string, S, N>;
  ids<E extends string>(ids: EARS.EntityId<E> | readonly EARS.EntityId<E>[]): QueryBuilder<E, S, N>;
  all(): QueryBuilder<string, S, N>;
}

/** Creates an entity of a declared type; writes to its declared fields are checked */
export interface TypedCreate<S extends EntityShapes, N extends string = string> {
  <E extends string>(type: Name<N, E>, options?: CreateOptions): WriteBuilder<E, S>;
}

/** Writes to the entity with `id`, creating it as `type` when no entity has it */
export interface TypedUpsert<S extends EntityShapes, N extends string = string> {
  <E extends string>(type: Name<N, E>, id: EARS.EntityId): WriteBuilder<E, S>;
}

/** Writes to an existing entity; an id tagged with its type checks writes to declared fields, a plain one doesn't */
export interface TypedWrite<S extends EntityShapes> {
  <E extends string>(id: EARS.EntityId<E>): WriteBuilder<E, S>;
}

export interface TypedFindById<S extends EntityShapes> {
  <E extends string>(id: EARS.EntityId<E>): ShapeOf<S, E> | undefined;
  <T>(id: EARS.EntityId): T | undefined;
}

// The `<T>` overloads read rows as an explicit shape. A call without a type argument that fails the
// first overload infers `E` here too, so an undeclared literal is still rejected; passing `T` leaves
// `E` at its default (`string`), which lets a name known only at runtime through.
export interface TypedFindAll<S extends EntityShapes, N extends string = string> {
  <E extends string>(entityType: Name<N, E>): ShapeOf<S, E>[];
  <T, E extends string = string>(entityType: Name<N, E>): T[];
}

export interface TypedFindWhere<S extends EntityShapes, N extends string = string> {
  <E extends string>(entityType: Name<N, E>, field: string, value: unknown): ShapeOf<S, E>[];
  <T, E extends string = string>(entityType: Name<N, E>, field: string, value: unknown): T[];
}

export interface TypedFindFirst<S extends EntityShapes, N extends string = string> {
  <E extends string>(entityType: Name<N, E>, field: string, value: unknown): ShapeOf<S, E> | undefined;
  <T, E extends string = string>(entityType: Name<N, E>, field: string, value: unknown): T | undefined;
}

/** Brands the id with the entity type when `S` declares its shape; other types stay unbranded. */
export type TypedCreateEntity<S extends EntityShapes, N extends string = string> =
  <E extends string>(t: Name<N, E>) => EARS.EntityId<E extends keyof S ? E : string>;

/** A field's value on an entity whose id names its type; `unknown` when the id isn't branded. */
export interface TypedGetAttr<S extends EntityShapes> {
  <E extends string, K extends keyof ShapeOf<S, E> & string>(id: EARS.EntityId<E>, field: K, index?: number): ShapeOf<S, E>[K] | null;
  (id: EARS.EntityId, field: string, index?: number): unknown;
}

/** Every value stored under a field (multi-valued attributes such as roles). */
export interface TypedGetAttrs<S extends EntityShapes> {
  <E extends string, K extends keyof ShapeOf<S, E> & string>(id: EARS.EntityId<E>, field: K): ShapeOf<S, E>[K][];
  (id: EARS.EntityId, field: string): unknown[];
}

export type TypedFindWithFields<S extends EntityShapes, N extends string = string> =
  <E extends string, K extends keyof ShapeOf<S, E> & string>(entityType: Name<N, E>, fields: readonly K[]) => Pick<ShapeOf<S, E>, K>[];

export type TypedFindByIdWithFields<S extends EntityShapes> =
  <E extends string, K extends keyof ShapeOf<S, E> & string>(id: EARS.EntityId<E>, fields: readonly K[]) => Pick<ShapeOf<S, E>, K> | undefined;

export type TypedFindWithRole<S extends EntityShapes, N extends string = string> =
  <E extends string>(entityType: Name<N, E>, role: string) => ShapeOf<S, E>[];

export type TypedFindFirstWithRole<S extends EntityShapes, N extends string = string> =
  <E extends string>(entityType: Name<N, E>, role: string) => ShapeOf<S, E> | undefined;

/** Creates an entity with a short code, label and timestamps filled in. */
export type TypedCreateEntityWithDefaults<S extends EntityShapes, N extends string = string> = <E extends string>(
  entityType: Name<N, E>,
  data: Partial<ShapeOf<S, E>>,
  prefix?: string,
  providedId?: EARS.EntityId,
) => ShapeOf<S, E> & CreatedEntityFields;

/** Writes the given fields (null drops one) and, unless skipped, `updatedAt`. */
export interface TypedUpdateEntity<S extends EntityShapes> {
  <E extends string>(id: EARS.EntityId<E>, updates: { [K in keyof ShapeOf<S, E>]?: ShapeOf<S, E>[K] | null }, skipTimestamp?: boolean): void;
}

export interface TypedEars<S extends EntityShapes, N extends string = string> {
  qx: TypedQx<S, N>;
  create: TypedCreate<S, N>;
  upsert: TypedUpsert<S, N>;
  write: TypedWrite<S>;
  transact: EarsQuery['transact'];
  findById: TypedFindById<S>;
  findByIdRaw: TypedFindById<S>;
  findAll: TypedFindAll<S, N>;
  findWhere: TypedFindWhere<S, N>;
  findFirst: TypedFindFirst<S, N>;
  findWithFields: TypedFindWithFields<S, N>;
  findByIdWithFields: TypedFindByIdWithFields<S>;
  findWithRole: TypedFindWithRole<S, N>;
  findFirstWithRole: TypedFindFirstWithRole<S, N>;
  createEntity: TypedCreateEntity<S, N>;
  createEntityWithDefaults: TypedCreateEntityWithDefaults<S, N>;
  updateEntity: TypedUpdateEntity<S>;
  getAttr: TypedGetAttr<S>;
  getAttrs: TypedGetAttrs<S>;
}

/** Calls `name` on the installed engine */
const onInstalled = <K extends keyof EarsQuery>(name: K) =>
  ((...args: unknown[]) => (installedEngine()[name] as (...a: unknown[]) => unknown)(...args));

/**
 * The EARS query helpers typed against `S`, taking the entity names `N` (any string when omitted).
 * Every pack gets functions acting on the installed engine; only their types differ.
 */
export function defineEars<S extends EntityShapes, N extends string = string>(): TypedEars<S, N> {
  return {
    qx: Object.assign(onInstalled('qx'), {
      ids: (ids: EARS.EntityId | readonly EARS.EntityId[]) => installedEngine().qx.ids(ids),
      all: () => installedEngine().qx.all(),
    }),
    create: onInstalled('create'),
    upsert: onInstalled('upsert'),
    write: onInstalled('write'),
    transact: onInstalled('transact'),
    findById: onInstalled('findById'),
    findByIdRaw: onInstalled('findByIdRaw'),
    findAll: onInstalled('findAll'),
    findWhere: onInstalled('findWhere'),
    findFirst: onInstalled('findFirst'),
    findWithFields: onInstalled('findWithFields'),
    findByIdWithFields: onInstalled('findByIdWithFields'),
    findWithRole: onInstalled('findWithRole'),
    findFirstWithRole: onInstalled('findFirstWithRole'),
    createEntity,
    createEntityWithDefaults: onInstalled('createEntityWithDefaults'),
    updateEntity: onInstalled('updateEntity'),
    getAttr: onInstalled('getAttr'),
    getAttrs: onInstalled('getAttrs'),
  } as unknown as TypedEars<S, N>;
}
