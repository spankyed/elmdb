import type { BatchResult, RelationRow } from 'elmdb';
import type { BatchSlot, BatchWriter, LinkOptions } from './batch.ts';
import type { BeforeCommitHook, EarsEngine, GetEdgesOptions } from './engine.ts';
import type { EntityRow, QueryBuilder, SourceOptions } from './query.ts';

/**
 * Compile-time branded entity ID utilities (`EARS.EntityId<'Task'>`).
 * Zero runtime overhead — branded IDs are plain strings at runtime.
 */
export namespace EARS {
  declare const entityTypeBrand: unique symbol;

  /** Branded external entity ID carrying its entity type name at compile time. */
  export type EntityId<TName extends string = string> = string & {
    readonly [entityTypeBrand]?: TName;
  };
}

/** Casts a raw string ID into a branded `EARS.EntityId<TName>` at zero runtime cost. */
export function asEntityId<TName extends string>(id: string): EARS.EntityId<TName> {
  return id as EARS.EntityId<TName>;
}

/** Compile-time schema contract accepted by `defineEars<TSchema>`. */
export interface EarsSchemaShape {
  readonly entities: { readonly [typeName: string]: Record<string, unknown> };
  readonly relations?: string;
}

/** Entity type names declared in `TSchema`. */
export type EntityTypeName<TSchema extends EarsSchemaShape> = Extract<keyof TSchema['entities'], string>;

/** Relation kind names declared in `TSchema` (defaults to `string` when omitted). */
export type RelationKindName<TSchema extends EarsSchemaShape> =
  TSchema['relations'] extends string ? TSchema['relations'] : string;

/** Strongly typed entity row for entity type `TName` in `TSchema`. */
export type TypedEntityRow<
  TSchema extends EarsSchemaShape,
  TName extends EntityTypeName<TSchema>,
> = {
  readonly id: EARS.EntityId<TName>;
  readonly typeName: TName;
  readonly trashed: boolean;
  readonly fields: Readonly<TSchema['entities'][TName]>;
};

/** Strongly typed `BatchWriter` scoped to `TSchema`. */
export interface TypedBatchWriter<TSchema extends EarsSchemaShape> {
  create<TName extends EntityTypeName<TSchema>>(
    typeName: TName,
    fields: TSchema['entities'][TName],
    externalId?: EARS.EntityId<TName> | string,
  ): BatchSlot;
  patch<TName extends EntityTypeName<TSchema>>(
    target: EARS.EntityId<TName> | BatchSlot,
    fields: Partial<TSchema['entities'][TName]>,
  ): this;
  unset<TName extends EntityTypeName<TSchema>>(
    target: EARS.EntityId<TName> | BatchSlot,
    fields: ReadonlyArray<Extract<keyof TSchema['entities'][TName], string>>,
  ): this;
  trash(target: EARS.EntityId<EntityTypeName<TSchema>> | BatchSlot): this;
  restore(target: EARS.EntityId<EntityTypeName<TSchema>> | BatchSlot): this;
  destroy(target: EARS.EntityId<EntityTypeName<TSchema>> | BatchSlot): this;
  link(
    source: EARS.EntityId<EntityTypeName<TSchema>> | BatchSlot,
    relKind: RelationKindName<TSchema>,
    target: EARS.EntityId<EntityTypeName<TSchema>> | BatchSlot,
    opts?: LinkOptions,
  ): this;
  unlink(
    source: EARS.EntityId<EntityTypeName<TSchema>> | BatchSlot,
    relKind?: RelationKindName<TSchema>,
    target?: EARS.EntityId<EntityTypeName<TSchema>> | BatchSlot,
  ): this;
  readonly length: number;
}

/** Strongly typed query entrypoints scoped to `TSchema`. */
export interface TypedQueryRoot<TSchema extends EarsSchemaShape> {
  of<TName extends EntityTypeName<TSchema>>(
    typeName: TName,
    opts?: SourceOptions,
  ): QueryBuilder<TypedEntityRow<TSchema, TName>>;
  ofTypes<TName extends EntityTypeName<TSchema>>(
    typeNames: readonly TName[],
    opts?: SourceOptions,
  ): QueryBuilder<TypedEntityRow<TSchema, TName>>;
  allEntities(opts?: SourceOptions): QueryBuilder<EntityRow>;
  ids<TName extends EntityTypeName<TSchema>>(
    ids: ReadonlyArray<EARS.EntityId<TName>>,
  ): QueryBuilder<TypedEntityRow<TSchema, TName>>;
}

/** Strongly typed EARS facade returned by `defineEars<TSchema>(getEngine)`. */
export interface TypedEars<TSchema extends EarsSchemaShape> {
  readonly engine: () => EarsEngine;
  readonly query: TypedQueryRoot<TSchema>;
  get<TName extends EntityTypeName<TSchema>>(
    id: EARS.EntityId<TName>,
  ): TypedEntityRow<TSchema, TName> | null;
  getEdges(
    id: EARS.EntityId<EntityTypeName<TSchema>>,
    opts?: GetEdgesOptions,
  ): RelationRow[];
  create<TName extends EntityTypeName<TSchema>>(
    typeName: TName,
    fields: TSchema['entities'][TName],
    externalId?: EARS.EntityId<TName> | string,
  ): EARS.EntityId<TName>;
  patch<TName extends EntityTypeName<TSchema>>(
    id: EARS.EntityId<TName>,
    fields: Partial<TSchema['entities'][TName]>,
  ): BatchResult;
  unset<TName extends EntityTypeName<TSchema>>(
    id: EARS.EntityId<TName>,
    fields: ReadonlyArray<Extract<keyof TSchema['entities'][TName], string>>,
  ): BatchResult;
  trash(id: EARS.EntityId<EntityTypeName<TSchema>>): BatchResult;
  restore(id: EARS.EntityId<EntityTypeName<TSchema>>): BatchResult;
  destroy(id: EARS.EntityId<EntityTypeName<TSchema>>): BatchResult;
  link(
    sourceId: EARS.EntityId<EntityTypeName<TSchema>>,
    relKind: RelationKindName<TSchema>,
    targetId: EARS.EntityId<EntityTypeName<TSchema>>,
    opts?: LinkOptions,
  ): BatchResult;
  unlink(
    sourceId: EARS.EntityId<EntityTypeName<TSchema>>,
    relKind?: RelationKindName<TSchema>,
    targetId?: EARS.EntityId<EntityTypeName<TSchema>>,
  ): BatchResult;
  batch(fn: (batch: TypedBatchWriter<TSchema>) => void): BatchResult;
  beforeCommit(
    typeNames: readonly EntityTypeName<TSchema>[] | null,
    callback: BeforeCommitHook,
  ): () => void;
}

/**
 * Creates a strongly-typed facade over a lazily-resolved `EarsEngine` getter.
 * Holds zero module-level state: callers control the engine lifecycle and binding.
 */
export function defineEars<TSchema extends EarsSchemaShape>(
  getEngine: () => EarsEngine,
): TypedEars<TSchema> {
  const query: TypedQueryRoot<TSchema> = {
    of<TName extends EntityTypeName<TSchema>>(typeName: TName, opts?: SourceOptions) {
      return getEngine().query.of(typeName, opts) as unknown as QueryBuilder<
        TypedEntityRow<TSchema, TName>
      >;
    },
    ofTypes<TName extends EntityTypeName<TSchema>>(typeNames: readonly TName[], opts?: SourceOptions) {
      return getEngine().query.ofTypes(typeNames, opts) as unknown as QueryBuilder<
        TypedEntityRow<TSchema, TName>
      >;
    },
    allEntities(opts?: SourceOptions) {
      return getEngine().query.allEntities(opts);
    },
    ids<TName extends EntityTypeName<TSchema>>(ids: ReadonlyArray<EARS.EntityId<TName>>) {
      return getEngine().query.ids(ids) as unknown as QueryBuilder<TypedEntityRow<TSchema, TName>>;
    },
  };

  return {
    engine: getEngine,
    query,
    get<TName extends EntityTypeName<TSchema>>(id: EARS.EntityId<TName>) {
      return getEngine().get(id) as TypedEntityRow<TSchema, TName> | null;
    },
    getEdges(id, opts) {
      return getEngine().getEdges(id, opts);
    },
    create<TName extends EntityTypeName<TSchema>>(
      typeName: TName,
      fields: TSchema['entities'][TName],
      externalId?: EARS.EntityId<TName> | string,
    ): EARS.EntityId<TName> {
      return getEngine().create(typeName, fields, externalId) as EARS.EntityId<TName>;
    },
    patch(id, fields) {
      return getEngine().patch(id, fields as Record<string, unknown>);
    },
    unset(id, fields) {
      return getEngine().unset(id, fields);
    },
    trash(id) {
      return getEngine().trash(id);
    },
    restore(id) {
      return getEngine().restore(id);
    },
    destroy(id) {
      return getEngine().destroy(id);
    },
    link(sourceId, relKind, targetId, opts) {
      return getEngine().link(sourceId, relKind, targetId, opts);
    },
    unlink(sourceId, relKind, targetId) {
      return getEngine().unlink(sourceId, relKind, targetId);
    },
    batch(fn) {
      return getEngine().batch((raw: BatchWriter) => {
        fn(raw as unknown as TypedBatchWriter<TSchema>);
      });
    },
    beforeCommit(typeNames, callback) {
      return getEngine().beforeCommit(typeNames, callback);
    },
  };
}
