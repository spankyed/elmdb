import type { Create } from './write.ts';
import { getTimestamp } from './entity-utils.ts';
import type { createEntityCounters } from './entity-utils.ts';
import { EARS } from './entities.ts';

/** Fields createEntityWithDefaults always writes, on top of the caller's data. */
export interface CreatedEntityFields {
  id: EARS.EntityId;
  entityType: EARS.Entity;
  shortCode: string;
  label: string;
  createdAt: number;
  updatedAt: number;
}

/** An engine's entity helpers, over its writes */
export function createEntityHelpers({ create, counters }: { create: Create; counters: ReturnType<typeof createEntityCounters> }) {
  /**
   * T is the caller's own attribute shape — it need not declare the fields this
   * function fills in (entityType, shortCode, label, timestamps); those are added
   * to the return type instead.
   */
  function createEntityWithDefaults<T extends Record<string, unknown> = Record<string, unknown>>(
    entityType: EARS.Entity,
    data: Partial<T> & { id?: EARS.EntityId },
    prefix?: string,
  ): T & CreatedEntityFields {
    const ts = getTimestamp();
    const shortCode = data.shortCode || counters.generateShortCode(entityType, prefix || entityType.substring(0, 3).toUpperCase());
    const label = data.label || counters.generateLabelWithCount(`New ${entityType}`, entityType);

    const fields = { ...data, shortCode, label };
    const id = create(entityType, { ...fields, createdAt: ts }).id();

    return { ...fields, entityType, createdAt: ts, updatedAt: ts, id } as unknown as T & CreatedEntityFields;
  }

  return { createEntityWithDefaults };
}
