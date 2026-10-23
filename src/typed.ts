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
import type { CreateFields, QueryBuilder, RequiredField, RowIdentity, WriteBuilder, WriteOptions } from './runtime.ts';
import { newId } from './attribute-storage.ts';
import type { Ears } from './engine.ts';

/** An entity name argument: a declared name, or a name only known at runtime (see EntityNameArg) */
type Name<N extends string, E extends string> = EntityNameArg<N, E>;

/**
 * The query entry points: `query(type)` from a declared entity type (its name checked, its rows typed), `query.ids`
 * from ids (typed by the entity an id is tagged with), `query.all` from every entity. A name and an id are
 * different arguments to different functions, so an id passed where a type is meant fails to compile.
 */
export interface TypedQueryEntries<S extends EntityShapes, N extends string = string, R extends string = string, O extends string = string> {
  <E extends string>(type: Name<N, E>): QueryBuilder<E, S, N, R, O>;
  <E extends string>(types: readonly Name<N, E>[]): QueryBuilder<string, S, N, R, O>;
  ids<E extends string>(ids: EARS.EntityId<E> | readonly EARS.EntityId<E>[]): QueryBuilder<E, S, N, R, O>;
  all(): QueryBuilder<string, S, N, R, O>;
}

/** The typed queries, which reach no entity in the trash; `withTrashed` reaches those too, `trashed` only those */
export interface TypedQuery<S extends EntityShapes, N extends string = string, R extends string = string, O extends string = string> extends TypedQueryEntries<S, N, R, O> {
  withTrashed: TypedQueryEntries<S, N, R, O>;
  trashed: TypedQueryEntries<S, N, R, O>;
}

/** The argument after a type: required when the shape requires a field, so an incomplete row doesn't compile */
type RowArg<S extends EntityShapes, E extends string, Row> = [RequiredField<S, E>] extends [never] ? [row?: Row] : [row: Row];

/**
 * Creates an entity of a declared type with its row: every field the shape requires and any it allows, beside the id
 * and times it keeps when it brings them (`RowIdentity`) — one argument, so nothing lands in the wrong place
 */
export interface TypedCreate<S extends EntityShapes, N extends string = string, R extends string = string, O extends string = string> {
  <E extends string>(type: Name<N, E>, ...row: RowArg<S, E, CreateFields<S, E> & RowIdentity>): WriteBuilder<E, S, R, O>;
}

/** Writes the row's fields to the entity with `id`, creating it as `type` with them when no entity has it */
export interface TypedUpsert<S extends EntityShapes, N extends string = string, R extends string = string, O extends string = string> {
  <E extends string>(type: Name<N, E>, id: EARS.EntityId, ...row: RowArg<S, E, CreateFields<S, E>>): WriteBuilder<E, S, R, O>;
}

/** Writes to an existing entity; an id tagged with its type checks writes to declared fields, a plain one doesn't */
export interface TypedWrite<S extends EntityShapes, R extends string = string, O extends string = string> {
  <E extends string>(id: EARS.EntityId<E>, options?: WriteOptions): WriteBuilder<E, S, R, O>;
}

/** Brands the id with the entity type when `S` declares its shape; other types stay unbranded. */
export type TypedNewId<S extends EntityShapes, N extends string = string> =
  <E extends string>(t: Name<N, E>) => EARS.EntityId<E extends keyof S ? E : string>;

/**
 * One field of an entity's row, read as `getRow` reads it (the trash included), or undefined when it holds none: the
 * field typed by a branded id's shape, and `unknown` for an id with no type in it
 */
export interface TypedGetAttr<S extends EntityShapes> {
  <E extends string, K extends keyof ShapeOf<S, E> & string>(id: EARS.EntityId<E>, field: K): ShapeOf<S, E>[K] | undefined;
  (id: EARS.EntityId, field: string): unknown;
}

export interface TypedEars<S extends EntityShapes, N extends string = string, R extends string = string, O extends string = string> {
  query: TypedQuery<S, N, R, O>;
  create: TypedCreate<S, N, R, O>;
  upsert: TypedUpsert<S, N, R, O>;
  write: TypedWrite<S, R, O>;
  transact: Ears['transact'];
  newId: TypedNewId<S, N>;
  getAttr: TypedGetAttr<S>;
}

/**
 * The EARS query helpers typed against `S`, taking the entity names `N`, the relation kinds `R` and the roles `O` (any
 * string when omitted), each acting on the engine `engine` returns when it is called. A getter rather than an engine,
 * so a facade can be defined before its engine exists, which is the case for every pack: its `#generated/ears`
 * passes the SDK's `boundEars`.
 */
export function defineEars<S extends EntityShapes, N extends string = string, R extends string = string, O extends string = string>(engine: () => Ears): TypedEars<S, N, R, O> {
  const on = <K extends keyof Ears>(name: K) =>
    ((...args: unknown[]) => (engine()[name] as (...a: unknown[]) => unknown)(...args));
  return {
    query: Object.assign(on('query'), {
      ids: (ids: EARS.EntityId | readonly EARS.EntityId[]) => engine().query.ids(ids),
      all: () => engine().query.all(),
      withTrashed: Object.assign((start: never) => engine().query.withTrashed(start), {
        ids: (ids: EARS.EntityId | readonly EARS.EntityId[]) => engine().query.withTrashed.ids(ids),
        all: () => engine().query.withTrashed.all(),
      }),
      trashed: Object.assign((start: never) => engine().query.trashed(start), {
        ids: (ids: EARS.EntityId | readonly EARS.EntityId[]) => engine().query.trashed.ids(ids),
        all: () => engine().query.trashed.all(),
      }),
    }),
    create: on('create'),
    upsert: on('upsert'),
    write: on('write'),
    transact: on('transact'),
    newId,
    getAttr: on('getAttr'),
  } as unknown as TypedEars<S, N, R, O>;
}
