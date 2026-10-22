import type { EARS } from './entities.ts';
import type { Query } from './query.ts';

export function getTimestamp(): number {
  return Date.now();
}

/** An engine's numbering of new entities, by how many of their type it holds */
export function createEntityCounters(query: Query) {
  function generateShortCode(entityType: EARS.Entity, prefix: string): string {
    const count = query(entityType as never).count() + 1;
    return `${prefix}-${count}`;
  }

  function generateLabelWithCount(baseLabel: string, entityType: EARS.Entity): string {
    const count = query(entityType as never).count() + 1;
    return `${baseLabel} ${count}`;
  }

  return { generateShortCode, generateLabelWithCount };
}

export function filterSystemFields<T extends object>(
  updates: T,
  additionalExcludes: string[] = []
): Partial<T> {
  const systemFields = ['id', 'entityType', 'createdAt', ...additionalExcludes];
  const filtered: Partial<T> = {};
  Object.entries(updates).forEach(([key, value]) => {
    if (!systemFields.includes(key) && value !== undefined) {
      filtered[key as keyof T] = value as T[keyof T];
    }
  });
  return filtered;
}
