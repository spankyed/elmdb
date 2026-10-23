import type { Query } from './query.ts';
import type { QueryBuilder } from './runtime.ts';
import type { EARS } from './entities.ts';

/**
 * An engine's untyped finders, over its queries, which reach no entity in the trash: rows by id, of a type, and of a
 * type with a field's value. Anything narrower is a query (`pick`, `withRole`, `first`, `count`, `exists`). Pack code
 * gets them typed from its #generated/ears (defineEars).
 */
export function createQueryHelpers(query: Query) {
  // A type known only at runtime: the finders are untyped, and so is the builder they start
  const ofType = (entityType: EARS.Entity) => query(entityType) as unknown as QueryBuilder;
  function findById<T = unknown>(id: EARS.EntityId): T | undefined {
    return query.ids(id).pickAll()[0] as T | undefined;
  }

  function findAll<T = unknown>(entityType: EARS.Entity): T[] {
    return ofType(entityType).pickAll() as T[];
  }

  function findWhere<T = unknown>(entityType: EARS.Entity, field: string, value: unknown): T[] {
    return ofType(entityType).where(field, value).pickAll() as T[];
  }

  return { findById, findAll, findWhere };
}
