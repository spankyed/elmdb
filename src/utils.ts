export const isPlainObject = (val: unknown): val is Record<string, unknown> =>
  typeof val === 'object' && val !== null && !Array.isArray(val);

const random64 = (): string => crypto.getRandomValues(new BigUint64Array(1))[0].toString(36);

/** A new entity id's suffix: a base-36 timestamp and 64 random bits */
export function idSuffix(): string {
  return Date.now().toString(36) + random64();
}

/**
 * Whether two stored values are the same value: equal primitives, Dates at the same time, lists equal element by
 * element, and objects with equal fields whatever their order. A field holding `undefined` counts as absent, as it
 * is once stored.
 */
export function sameValue(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (a instanceof Date || b instanceof Date) return a instanceof Date && b instanceof Date && a.getTime() === b.getTime();
  if (Array.isArray(a) || Array.isArray(b)) {
    return Array.isArray(a) && Array.isArray(b) && a.length === b.length && a.every((v, i) => sameValue(v, b[i]));
  }
  if (!isPlainObject(a) || !isPlainObject(b)) return false;
  const fields = (o: Record<string, unknown>) => Object.keys(o).filter((k) => o[k] !== undefined);
  const [ka, kb] = [fields(a), fields(b)];
  return ka.length === kb.length && ka.every((k) => sameValue(a[k], b[k]));
}
