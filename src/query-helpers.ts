import type { qx as Qx } from './query.ts';
import type { EARS } from './entities.ts';
import { installedEngine } from './installed.ts';

const isDeleted = (entity: any): boolean => entity?.deleted === true;

/**
 * An engine's untyped finders, over its queries. Soft-deleted rows (`deleted: true`) are skipped except by
 * `findByIdRaw`. Pack code gets them typed from its #generated/ears (defineEars).
 */
export function createQueryHelpers(qx: typeof Qx) {
  function findById<T = unknown>(id: EARS.EntityId): T | undefined {
    const entity = qx([id]).pickAll()[0] as any;
    if (isDeleted(entity)) return undefined;
    return entity;
  }

  function findByIdRaw<T = unknown>(id: EARS.EntityId): T | undefined {
    return qx([id]).pickAll()[0] as T | undefined;
  }

  function findAll<T = unknown>(entityType: EARS.Entity): T[] {
    return (qx(entityType).pickAll() as any[]).filter(entity => !isDeleted(entity));
  }

  function findWhere<T = unknown>(entityType: EARS.Entity, field: string, value: unknown): T[] {
    return (qx(entityType).where(field, value).pickAll() as any[]).filter(entity => !isDeleted(entity));
  }

  function findFirst<T = unknown>(entityType: EARS.Entity, field: string, value: unknown): T | undefined {
    return findWhere<T>(entityType, field, value)[0];
  }

  function findWithFields<T>(entityType: EARS.Entity, fields: string[]): T[] {
    return (qx(entityType).pick(fields) as T[]).filter(entity => !isDeleted(entity));
  }

  function findByIdWithFields<T>(id: EARS.EntityId, fields: string[]): T | undefined {
    const entity = qx([id]).pick(fields)[0] as T | undefined;
    if (isDeleted(entity)) return undefined;
    return entity;
  }

  function countEntities(entityType: EARS.Entity): number {
    return findAll(entityType).length;
  }

  function exists(id: EARS.EntityId): boolean {
    return qx([id]).count() > 0;
  }

  function findWithRole<T>(entityType: EARS.Entity, role: string): T[] {
    return (qx(entityType).withRole(role).pickAll() as T[]).filter(entity => !isDeleted(entity));
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

// The installed engine's finders that packs import untyped (see installed.ts)

export function countEntities(entityType: EARS.Entity): number {
  return installedEngine().countEntities(entityType);
}

export function exists(id: EARS.EntityId): boolean {
  return installedEngine().exists(id);
}

export function hasIdCollision(providedId: string | undefined): boolean {
  return installedEngine().hasIdCollision(providedId);
}
