import { EARS } from './entities.ts';
import type { AttributeStorage } from './attribute-storage.ts';

export function getTimestamp(): number {
  return Date.now();
}

/**
 * The engine's own entity type for numbering: one row per sequence, `Sequence-<key>`, holding the next number.
 * No pack declares it and no query by type reaches it; `query.all()` leaves it out, as it does relations.
 */
export const SEQUENCE_TYPE = 'Sequence' as EARS.Entity;

const NEXT = EARS.AttrKind.Custom('next');

/**
 * An engine's numbering of new entities. A number is taken from a stored sequence, so it is never handed out
 * twice: destroying the row that holds the highest does not give its number back. A sequence's first use starts
 * after the highest number its type's rows already hold, and a number a row took for itself is skipped. The
 * sequence is written like any attribute, so it persists and a unit that throws returns its numbers.
 */
export function createEntityCounters(storage: AttributeStorage) {
  const { hasEntity, getAttr, putAttr, entityCreated, getEntitiesOfType } = storage;

  /** The numbers `field` holds across the type's rows, read as `<head><n>` */
  const numbersIn = (entityType: EARS.Entity, field: string, head: string) => {
    const kind = EARS.AttrKind.Custom(field);
    const taken = new Set<number>();
    for (const id of getEntitiesOfType(entityType)) {
      const value = getAttr(id, kind);
      if (typeof value !== 'string' || !value.startsWith(head)) continue;
      const rest = value.slice(head.length);
      if (/^\d+$/.test(rest)) taken.add(Number(rest));
    }
    return taken;
  };

  function take(key: string, entityType: EARS.Entity, field: string, head: string): string {
    const id = `${SEQUENCE_TYPE}-${key}` as EARS.EntityId;
    const taken = numbersIn(entityType, field, head);
    let next: number;
    if (hasEntity(id)) {
      next = getAttr(id, NEXT) as number;
    } else {
      next = Math.max(0, ...taken) + 1;
      entityCreated(id, SEQUENCE_TYPE);
    }
    while (taken.has(next)) next++;
    putAttr(id, NEXT, next + 1);
    return `${head}${next}`;
  }

  /** `<prefix>-<n>`, numbered per type and prefix */
  const generateShortCode = (entityType: EARS.Entity, prefix: string): string =>
    take(`${entityType}:${prefix}`, entityType, 'shortCode', `${prefix}-`);

  /** `<baseLabel> <n>`, numbered per type and base */
  const generateLabelWithCount = (baseLabel: string, entityType: EARS.Entity): string =>
    take(`label:${entityType}:${baseLabel}`, entityType, 'label', `${baseLabel} `);

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
