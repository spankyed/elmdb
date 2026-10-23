import type { Query } from './query.ts';
import type { QueryBuilder } from './runtime.ts';
import type { EARS } from './entities.ts';

/**
 * An engine's untyped finders, over its queries, which reach no entity in the trash. Pack code gets them typed from
 * its #generated/ears (defineEars).
 */
export function createQueryHelpers(query: Query) {
  // A type known only at runtime: the finders are untyped, and so is the builder they start
  const ofType = (entityType: EARS.Entity) => query(entityType as never) as unknown as QueryBuilder;
  function findById<T = unknown>(id: EARS.EntityId): T | undefined {
    return query.ids(id).pickAll()[0] as T | undefined;
  }

  function findAll<T = unknown>(entityType: EARS.Entity): T[] {
    return ofType(entityType).pickAll() as T[];
  }

  function findWhere<T = unknown>(entityType: EARS.Entity, field: string, value: unknown): T[] {
    return ofType(entityType).where(field, value).pickAll() as T[];
  }

  function findFirst<T = unknown>(entityType: EARS.Entity, field: string, value: unknown): T | undefined {
    return findWhere<T>(entityType, field, value)[0];
  }

  function findWithFields<T>(entityType: EARS.Entity, fields: string[]): T[] {
    return ofType(entityType).pick(fields) as T[];
  }

  function findByIdWithFields<T>(id: EARS.EntityId, fields: string[]): T | undefined {
    return query.ids(id).pick(fields)[0] as T | undefined;
  }

  function countEntities(entityType: EARS.Entity): number {
    return ofType(entityType).count();
  }

  function exists(id: EARS.EntityId): boolean {
    return query.ids(id).exists();
  }

  function findWithRole<T>(entityType: EARS.Entity, role: string): T[] {
    return ofType(entityType).withRole(role).pickAll() as T[];
  }

  function findFirstWithRole<T>(entityType: EARS.Entity, role: string): T | undefined {
    return findWithRole<T>(entityType, role)[0];
  }

  /** Whether an entity has the id, in the trash or not */
  function hasIdCollision(providedId: string | undefined): boolean {
    if (!providedId) return false;
    return query.withTrashed.ids(providedId as EARS.EntityId).exists();
  }

  return {
    findById, findAll, findWhere, findFirst, findWithFields, findByIdWithFields,
    countEntities, exists, findWithRole, findFirstWithRole, hasIdCollision,
  };
}
