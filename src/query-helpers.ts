import type { Qx } from './query.ts';
import type { QueryBuilder } from './runtime.ts';
import type { EARS } from './entities.ts';

const isDeleted = (entity: any): boolean => entity?.deleted === true;

/**
 * An engine's untyped finders, over its queries. Soft-deleted rows (`deleted: true`) are skipped except by
 * `findByIdRaw`. Pack code gets them typed from its #generated/ears (defineEars).
 */
export function createQueryHelpers(qx: Qx) {
  // A type known only at runtime: the finders are untyped, and so is the builder they start
  const ofType = (entityType: EARS.Entity) => qx(entityType as never) as unknown as QueryBuilder;
  function findById<T = unknown>(id: EARS.EntityId): T | undefined {
    const entity = qx.ids(id).pickAll()[0] as any;
    if (isDeleted(entity)) return undefined;
    return entity;
  }

  function findByIdRaw<T = unknown>(id: EARS.EntityId): T | undefined {
    return qx.ids(id).pickAll()[0] as T | undefined;
  }

  function findAll<T = unknown>(entityType: EARS.Entity): T[] {
    return (ofType(entityType).pickAll() as any[]).filter(entity => !isDeleted(entity));
  }

  function findWhere<T = unknown>(entityType: EARS.Entity, field: string, value: unknown): T[] {
    return (ofType(entityType).where(field, value).pickAll() as any[]).filter(entity => !isDeleted(entity));
  }

  function findFirst<T = unknown>(entityType: EARS.Entity, field: string, value: unknown): T | undefined {
    return findWhere<T>(entityType, field, value)[0];
  }

  function findWithFields<T>(entityType: EARS.Entity, fields: string[]): T[] {
    return (ofType(entityType).pick(fields) as T[]).filter(entity => !isDeleted(entity));
  }

  function findByIdWithFields<T>(id: EARS.EntityId, fields: string[]): T | undefined {
    const entity = qx.ids(id).pick(fields)[0] as T | undefined;
    if (isDeleted(entity)) return undefined;
    return entity;
  }

  function countEntities(entityType: EARS.Entity): number {
    return findAll(entityType).length;
  }

  function exists(id: EARS.EntityId): boolean {
    return qx.ids(id).count() > 0;
  }

  function findWithRole<T>(entityType: EARS.Entity, role: string): T[] {
    return (ofType(entityType).withRole(role).pickAll() as T[]).filter(entity => !isDeleted(entity));
  }

  function findFirstWithRole<T>(entityType: EARS.Entity, role: string): T | undefined {
    return findWithRole<T>(entityType, role)[0];
  }

  function hasIdCollision(providedId: string | undefined): boolean {
    if (!providedId) return false;
    return exists(providedId as EARS.EntityId);
  }

  return {
    findById, findByIdRaw, findAll, findWhere, findFirst, findWithFields, findByIdWithFields,
    countEntities, exists, findWithRole, findFirstWithRole, hasIdCollision,
  };
}
