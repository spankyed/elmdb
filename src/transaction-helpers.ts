import type { tx as Tx } from './transaction.ts';
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

/** An engine's entity and relation helpers, over its transactions */
export function createTransactionHelpers({ tx, counters }: { tx: typeof Tx; counters: ReturnType<typeof createEntityCounters> }) {
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

    const entity: Omit<T, 'id'> = {
      ...data,
      entityType,
      shortCode,
      label,
      createdAt: ts,
      updatedAt: ts,
    } as Omit<T, 'id'>;

    const id = providedId
      ? tx(providedId, true).batchPut(entity).id()
      : tx(entityType).batchPut(entity).id();

    return { ...entity, id } as unknown as T & CreatedEntityFields;
  }

  function updateEntity(
    id: EARS.EntityId,
    updates: Record<string, unknown>,
    skipTimestamp?: boolean
  ): void {
    const transaction = tx(id);

    if (!skipTimestamp) {
      transaction.merge('updatedAt', getTimestamp());
    }

    Object.entries(updates).forEach(([key, value]) => {
      if (value === undefined) return;
      if (value === null) {
        transaction.drop(EARS.AttrKind.Custom(key));
      } else if (Array.isArray(value)) {
        transaction.drop(EARS.AttrKind.Custom(key));
        transaction.put(key, value);
      } else {
        transaction.update(EARS.AttrKind.Custom(key), value);
      }
    });
  }

  function createRelation(
    sourceId: EARS.EntityId,
    relationType: EARS.RelKind,
    targetId: EARS.EntityId
  ): void {
    tx(sourceId).link(relationType, targetId);
  }

  function removeRelation(
    sourceId: EARS.EntityId,
    relationType: EARS.RelKind,
    targetId?: EARS.EntityId
  ): void {
    tx(sourceId).unlinkIf(relationType, targetId);
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
