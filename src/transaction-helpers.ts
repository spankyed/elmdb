import type { Create, Write } from './write.ts';
import { getTimestamp } from './entity-utils.ts';
import type { createEntityCounters } from './entity-utils.ts';
import { EARS } from './entities.ts';
import { installedEngine } from './installed.ts';

export function prepareEntity<T extends { entityType: EARS.Entity }>(
  entityType: EARS.Entity,
  data: Partial<T>,
  defaults?: Partial<T>
): Omit<T, 'id'> {
  const ts = getTimestamp();
  return {
    ...defaults,
    ...data,
    entityType,
    createdAt: ts,
    updatedAt: ts,
  } as Omit<T, 'id'>;
}

/** Fields createEntityWithDefaults always writes, on top of the caller's data. */
export interface CreatedEntityFields {
  id: EARS.EntityId;
  entityType: EARS.Entity;
  shortCode: string;
  label: string;
  createdAt: number;
  updatedAt: number;
}

/** An engine's entity and relation helpers, over its writes */
export function createTransactionHelpers({ create, write, counters }: { create: Create; write: Write; counters: ReturnType<typeof createEntityCounters> }) {
  /**
   * T is the caller's own attribute shape — it need not declare the fields this
   * function fills in (entityType, shortCode, label, timestamps); those are added
   * to the return type instead.
   */
  function createEntityWithDefaults<T extends Record<string, unknown> = Record<string, unknown>>(
    entityType: EARS.Entity,
    data: Partial<T>,
    prefix?: string,
    providedId?: EARS.EntityId,
  ): T & CreatedEntityFields {
    const ts = getTimestamp();
    const shortCode = data.shortCode || counters.generateShortCode(entityType, prefix || entityType.substring(0, 3).toUpperCase());
    const label = data.label || counters.generateLabelWithCount(`New ${entityType}`, entityType);

    const fields = { ...data, shortCode, label, updatedAt: ts };
    const id = create(entityType as never, { id: providedId, createdAt: ts }).setMany(fields).id();

    return { ...fields, entityType, createdAt: ts, id } as unknown as T & CreatedEntityFields;
  }

  function updateEntity(
    id: EARS.EntityId,
    updates: Record<string, unknown>,
    skipTimestamp?: boolean
  ): void {
    const builder = write(id);

    if (!skipTimestamp) {
      builder.set('updatedAt', getTimestamp());
    }

    Object.entries(updates).forEach(([key, value]) => {
      if (value === undefined) return;
      if (value === null) builder.delete(key);
      else builder.set(key, value);
    });
  }

  function createRelation(
    sourceId: EARS.EntityId,
    relationType: EARS.RelKind,
    targetId: EARS.EntityId
  ): void {
    write(sourceId).link(relationType, targetId);
  }

  function removeRelation(
    sourceId: EARS.EntityId,
    relationType: EARS.RelKind,
    targetId?: EARS.EntityId
  ): void {
    write(sourceId).unlink(relationType, targetId);
  }

  return { createEntityWithDefaults, updateEntity, createRelation, removeRelation };
}

// The installed engine's relation helpers packs import (see installed.ts)

export function createRelation(
  sourceId: EARS.EntityId,
  relationType: EARS.RelKind,
  targetId: EARS.EntityId
): void {
  installedEngine().createRelation(sourceId, relationType, targetId);
}

export function removeRelation(
  sourceId: EARS.EntityId,
  relationType: EARS.RelKind,
  targetId?: EARS.EntityId
): void {
  installedEngine().removeRelation(sourceId, relationType, targetId);
}
