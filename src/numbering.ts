import { EARS } from './entities.ts';
import type { AttributeStorage } from './attribute-storage.ts';
import type { StoreAccess } from './store-access.ts';

export function getTimestamp(): number {
  return Date.now();
}

/**
 * An engine's numbering of new entities. A number is taken from a stored sequence (`['s', key]`), so it is never
 * handed out twice: destroying the row that holds the highest does not give its number back. A sequence's first use
 * starts after the highest number its type's rows already hold, and a number a row took for itself is skipped. The
 * sequence is written in the unit that takes it, so a unit that throws returns its numbers.
 */
export function createEntityCounters(storage: AttributeStorage, access: StoreAccess) {
  const { getAttr, getEntitiesOfType, holders, getSequence, setSequence } = storage;

  /** The numbers `field` holds across the type's rows, read as `<head><n>` */
  const numbersIn = (entityType: EARS.Entity, field: string, head: string) => {
    const kind = EARS.AttrKind.Custom(field);
    const numbers: number[] = [];
    for (const id of getEntitiesOfType(entityType)) {
      const value = getAttr(id, kind);
      if (typeof value !== 'string' || !value.startsWith(head)) continue;
      const rest = value.slice(head.length);
      if (/^\d+$/.test(rest)) numbers.push(Number(rest));
    }
    return numbers;
  };

  const take = (key: string, entityType: EARS.Entity, field: string, head: string): string => access.write(() => {
    const kind = EARS.AttrKind.Custom(field);
    // The one walk of the type: a sequence starts after the highest number its rows already hold
    let next = getSequence(key) ?? numbersIn(entityType, field, head).reduce((max, n) => Math.max(max, n), 0) + 1;
    // A number a row took for itself is skipped, which the type's index on the field answers without a walk
    while (holders(entityType, kind, `${head}${next}`).length > 0) next++;
    setSequence(key, next + 1);
    return `${head}${next}`;
  });

  /** `<prefix>-<n>`, numbered per type and prefix */
  const generateShortCode = (entityType: EARS.Entity, prefix: string): string =>
    take(`${entityType}:${prefix}`, entityType, 'shortCode', `${prefix}-`);

  /** `<base> <n>`, numbered per type and base */
  const nextLabel = (entityType: EARS.Entity, base: string): string =>
    take(`label:${entityType}:${base}`, entityType, 'label', `${base} `);

  return { generateShortCode, nextLabel };
}
