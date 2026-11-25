import type { EARS } from './entities.ts';

const random64 = (): string => crypto.getRandomValues(new BigUint64Array(1))[0]!.toString(36);

/** A new entity id's suffix: a base-36 timestamp and 64 random bits */
export function idSuffix(now: () => number = Date.now): string {
  return Math.floor(Math.max(0, now())).toString(36) + random64();
}

/**
 * Creates an id for a new entity of type `t`. A typed facade (`defineEars`) holds this as `newId`,
 * typed to brand the id with the entity type, so reads through its helpers infer the shape.
 *
 * @internal
 */
export const newId = <E extends EARS.Entity>(t: E, now?: () => number): EARS.EntityId =>
  `${t}-${idSuffix(now)}` as EARS.EntityId;

/**
 * An entity's type, from its id: the one record of it. `newId` builds `<Type>-<suffix>`, `create`
 * refuses an id naming another type, and the id is the wire's record too — a frontend opens an entity by
 * it, and reference nodes keep ids inside stored documents — so nothing else stores the type. The `entityType` field
 * every row carries is read from it.
 */
export const entityTypeOf = (id: EARS.EntityId) => {
  const dash = id.indexOf('-');
  return dash === -1 ? id as unknown as EARS.Entity : id.substring(0, dash) as EARS.Entity;
};

/**
 * Sets a field on an object a read builds: `__proto__` assigned would set the object's prototype rather than hold a
 * value, so it is defined as an own property
 */
export const setField = <V>(row: Record<string, V>, kind: string, value: V) => {
  if (kind === '__proto__') Object.defineProperty(row, kind, { value, writable: true, enumerable: true, configurable: true });
  else row[kind] = value;
};

export function getTimestamp(): number {
  return Date.now();
}
