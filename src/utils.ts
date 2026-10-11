export const isPlainObject = (val: unknown): val is Record<string, unknown> =>
  typeof val === 'object' && val !== null && !Array.isArray(val);

const random64 = (): string => crypto.getRandomValues(new BigUint64Array(1))[0].toString(36);

/** A new entity id's suffix: a base-36 timestamp and 64 random bits */
export function idSuffix(): string {
  return Date.now().toString(36) + random64();
}
