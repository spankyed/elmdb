/**
 * EARS query helpers typed against an entity shape map. This is the only pack-facing way
 * to get them: `apack pack generate-entries` writes `#generated/ears` with
 * `defineEars<PackSchema>()` (the pack's entities plus its dependencies'). There is no
 * global registry, so each pack's typing depends only on what it declares.
 *
 * CHANGE CONTROL: these types are a specified contract, and editor completions and error messages
 * depend on their exact form. Don't change them to make one call site compile; fix the call site.
 * Read packages/apack-sdk/TYPED-EARS.md (the contract and the pre-change checklist) first.
 * Overload order here is part of the contract: a signature that accepts more than the one before
 * it lets a call that should fail (a typo'd name or field) fall through to it.
 */
import type { EARS, EntityNameArg, EntityShapes, ShapeOf } from './entities.ts';
import type { CreateFields, EarsSchema, QueryBuilder, RequiredField, RowIdentity, ShortCodeIdentity, WriteBuilder, WriteOptions, WriteTarget } from './runtime.ts';
import { newId } from './ids.ts';
import type { Ears } from './engine.ts';

/** An entity name argument: a declared name, or a name only known at runtime (see EntityNameArg) */
type Name<N extends string, E extends string> = EntityNameArg<N, E>;

/**
 * The query entry points: `query(type)` from a declared entity type (its name checked, its rows typed), `query.ids`
 * from ids (typed by the entity an id is tagged with), `query.all` from every entity. A name and an id are
 * different arguments to different functions, so an id passed where a type is meant fails to compile.
 */
export interface TypedQueryEntries<D extends EarsSchema> {
  <E extends string>(type: Name<D['names'], E>): QueryBuilder<E, D>;
  <E extends string>(types: readonly Name<D['names'], E>[]): QueryBuilder<string, D>;
  ids<E extends string>(ids: EARS.EntityId<E> | readonly EARS.EntityId<E>[]): QueryBuilder<E, D>;
  all(): QueryBuilder<string, D>;
}

/** The typed queries, which reach no entity in the trash; `withTrashed` reaches those too, `trashed` only those */
export interface TypedQuery<D extends EarsSchema> extends TypedQueryEntries<D> {
  withTrashed: TypedQueryEntries<D>;
  trashed: TypedQueryEntries<D>;
}

/** The argument after a type: required when the shape requires a field, so an incomplete row doesn't compile */
type RowArg<S extends EntityShapes, E extends string, Row> = [RequiredField<S, E>] extends [never] ? [row?: Row] : [row: Row];

/**
 * Creates an entity of a declared type with its row: every field the shape requires and any it allows, beside the id
 * and times it keeps when it brings them (`RowIdentity`) — one argument, so nothing lands in the wrong place
 */
export interface TypedCreate<D extends EarsSchema> {
  <E extends string>(type: Name<D['names'], E>, ...row: RowArg<D['shapes'], E, CreateFields<D['shapes'], E> & RowIdentity & ShortCodeIdentity<D['shapes'], E>>): WriteBuilder<E, D>;
}

/**
 * Writes the row to the entity with its `id`, creating it as `type` with the row when no entity has it: `create`'s row,
 * with the id always given — one argument, as `create` takes
 */
export interface TypedUpsert<D extends EarsSchema> {
  <E extends string>(type: Name<D['names'], E>, row: CreateFields<D['shapes'], E> & RowIdentity & ShortCodeIdentity<D['shapes'], E> & { id: EARS.EntityId }): WriteBuilder<E, D>;
}

/** Writes to an existing entity; an id tagged with its type checks writes to declared fields, a plain one doesn't */
export interface TypedWrite<D extends EarsSchema> {
  <E extends string>(id: WriteTarget<E>, options?: WriteOptions): WriteBuilder<E, D>;
}

/** Brands the id with the entity type when the schema declares its shape; other types stay unbranded. */
export type TypedNewId<D extends EarsSchema> =
  <E extends string>(t: Name<D['names'], E>) => EARS.EntityId<E extends keyof D['shapes'] ? E : string>;

/**
 * One field of an entity's row, read as `getRow` reads it (the trash included), or undefined when it holds none: the
 * field typed by a branded id's shape, and `unknown` for an id with no type in it
 */
export interface TypedGetAttr<D extends EarsSchema> {
  <E extends string, K extends keyof ShapeOf<D['shapes'], E> & string>(id: EARS.EntityId<E>, field: K): ShapeOf<D['shapes'], E>[K] | undefined;
  (id: EARS.EntityId, field: string): unknown;
}

export interface TypedEars<D extends EarsSchema> {
  query: TypedQuery<D>;
  create: TypedCreate<D>;
  upsert: TypedUpsert<D>;
  write: TypedWrite<D>;
  transact: Ears['transact'];
  newId: TypedNewId<D>;
  getAttr: TypedGetAttr<D>;
}

/**
 * The EARS query helpers typed against the schema `D` — its entity shapes, and the entity names, relation kinds and
 * roles they take — each acting on the engine `engine` returns when it is called. A getter rather than an engine,
 * so a facade can be defined before its engine exists, which is the case for every pack: its `#generated/ears`
 * passes the SDK's `boundEars`.
 */
export function defineEars<D extends EarsSchema>(engine: () => Ears): TypedEars<D> {
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
  } as unknown as TypedEars<D>;
}
