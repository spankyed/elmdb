// The engine's writes: each builder call one of elmdb's batch operations (`elmdb/docs/engine.md`, Writes), sent as a
// batch of its own, in the unit `transact` holds open or as a unit of its own. elmdb holds each to its rules and
// refuses it whole; the builder checks only what can't cross, a value of `undefined`.
import type { EARS } from './entities.ts';
import { EarsError, RESERVED_ENTITY_TYPES, refuseReserved } from './errors.ts';
import type { NameChecks } from './names.ts';
import type { Op, OpResult } from './database.ts';
import type { LinkOptions, RowIdentity, WriteBuilder, WriteOptions, WriteTarget } from './runtime.ts';

const LINK_OPTIONS: readonly string[] = ['info'] satisfies Array<keyof LinkOptions>;

/** A string that is not shaped like an id: where a type is meant, an id fails to compile */
export type TypeName<T extends string> = T extends `${string}-${string}` ? never : T;

/** Creates an entity of a registered type with its row: its fields, and its id and times when it brings them */
export interface Create {
  <E extends string>(type: TypeName<E>, row?: Record<string, unknown> & RowIdentity): WriteBuilder<E>;
}

/**
 * Writes the row to the entity with its `id`, creating it as `type` with the row when no entity has it: the same row
 * `create` takes, with the id it is written under always given
 */
export interface Upsert {
  <E extends string>(type: TypeName<E>, row: Record<string, unknown> & RowIdentity & { id: EARS.EntityId }): WriteBuilder<E>;
}

/** Writes to the entity with this id, which must exist; a relation's id is refused (`WriteTarget`) */
export interface Write {
  <E extends string>(id: WriteTarget<E>, options?: WriteOptions): WriteBuilder<E>;
}

export interface WriteScope {
  /** Whether the database's schema declares the entity type */
  isEntityType(name: string): boolean;
  /** Sends a batch, stamped with `now` */
  write(ops: readonly Op[], now?: number): OpResult[];
  /** Whether an entity has the id, and whether a relation does */
  exists(id: string): { entity: boolean; relation: boolean };
  getRow(id: EARS.EntityId): Record<string, unknown> | undefined;
  names: NameChecks;
}

/** The fields of a row whose value isn't `undefined`, refusing an engine's own among the rest too, as `setMany` does */
function definedFields(fields: Record<string, unknown>, via: string): Record<string, unknown> {
  const defined: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(fields)) {
    if (value === undefined) refuseReserved(key, via);
    else Object.defineProperty(defined, key, { value, writable: true, enumerable: true, configurable: true });
  }
  return defined;
}

export function createWrites({ write: send, exists, getRow, names, isEntityType }: WriteScope) {
  const one = (op: Op, now?: number) => send([op], now)[0]!;

  /**
   * The writes to one entity. Its attribute writes stamp `updatedAt` unless `touch` is off — each with the time of the
   * builder's first, so a chain of them is one change — and links, roles and the trash don't.
   */
  function builder(id: EARS.EntityId, touch: boolean, changedAt?: number): WriteBuilder {
    let stampAt = changedAt;
    const attribute = (op: Op) => {
      stampAt ??= Date.now();
      one({ ...op, id, touch }, stampAt);
      return self;
    };
    const self: WriteBuilder = {
      set(k, v) {
        if (v === undefined) throw new EarsError('NO_VALUE', `set('${k}'): no value to store. delete('${k}') removes the attribute`);
        return attribute({ op: 'set', fields: definedFields({ [k]: v }, 'set') });
      },
      setMany(attrs) {
        const fields = definedFields(attrs as Record<string, unknown>, 'setMany');
        return Object.keys(fields).length ? attribute({ op: 'set', fields }) : self;
      },
      append: (k, v) => attribute({ op: 'append', field: k, value: v }),
      merge: (k, v) => attribute({ op: 'merge', field: k, value: v }),
      delete: (k) => attribute({ op: 'delete', field: k }),
      removeAt: (k, index) => attribute({ op: 'removeAt', field: k, index }),
      removeValue: (k, v) => attribute({ op: 'removeValue', field: k, value: v }),

      link(kind, target, options: LinkOptions = {}) {
        // An edge's info is `{ info }`: an object with other keys is info passed where the options go
        const stray = Object.keys(options).filter((k) => !LINK_OPTIONS.includes(k));
        if (stray.length) throw new Error(`link(${kind}): unknown option ${stray.join(', ')}; an edge's info goes in { info }`);
        names.relationKinds(kind, 'link');
        one({ op: 'link', source: id, kind, target, info: options.info });
        return self;
      },
      unlink(kind, target) {
        names.relationKinds(kind, 'unlink');
        one({ op: 'unlink', source: id, kind, target });
        return self;
      },
      unlinkAll() {
        one({ op: 'unlink', source: id });
        return self;
      },

      grant(role) {
        names.role(role, 'grant');
        one({ op: 'grant', id, role });
        return self;
      },
      revoke(role) {
        names.role(role, 'revoke');
        one({ op: 'revoke', id, role });
        return self;
      },
      grantExclusive(role, among) {
        names.role(role, 'grantExclusive');
        one({ op: 'grantExclusive', id, role, among: [...among] });
        return self;
      },

      trash(at = Date.now()) {
        one({ op: 'trash', id, at });
        return self;
      },
      restore() {
        one({ op: 'restore', id });
        return self;
      },

      destroy() {
        one({ op: 'destroy', id });
      },

      id: () => id,

      // The builder's entity exists — write refuses an id no entity has, and destroy ends the chain
      row: () => getRow(id) as ReturnType<WriteBuilder['row']>,
    };
    return self;
  }

  /** A brought short code is a string, which is all the protocol carries; elmdb holds it to the type's form */
  const refuseShortCode = (via: string, type: string, shortCode: unknown) => {
    if (shortCode === undefined || typeof shortCode === 'string') return;
    throw new EarsError('INVALID_SHORT_CODE', `${via}(${JSON.stringify(type)}, { shortCode: ${JSON.stringify(shortCode)} }): a short code is a string, <prefix>-<n>`);
  };

  /**
   * Refuses a name the schema doesn't declare, saying where an id goes when it is one. The engine's own types (a
   * relation's) are elmdb's to refuse, as no schema declares them
   */
  const requireType = (type: string, via: string) => {
    if ((Object.values(RESERVED_ENTITY_TYPES) as string[]).includes(type) || (typeof type === 'string' && isEntityType(type))) return;
    throw new EarsError(
      'UNKNOWN_ENTITY_TYPE',
      `${via}(${JSON.stringify(type)}): not a registered entity type.` +
        (typeof type === 'string' && type.includes('-') ? ' It looks like an id: use write(id).' : ''),
    );
  };

  const create = ((type: string, row: Record<string, unknown> & RowIdentity = {}) => {
    requireType(type, 'create');
    const { id: given, shortCode, createdAt, updatedAt, ...fields } = row;
    refuseShortCode('create', type, shortCode);
    const result = one({ op: 'create', type, id: given, fields: definedFields(fields, 'create'), shortCode, createdAt, updatedAt });
    // The creation is the entity's first change: its fields, and what the builder writes now, are part of it
    return builder((result as { id: EARS.EntityId }).id, false);
  }) as Create;

  const write = ((id: EARS.EntityId, { touch = true }: WriteOptions = {}) => {
    const held = exists(id);
    if (!held.entity) {
      // A relation's id reaches here only untyped (the compiler refuses one), so the refusal names where it goes
      throw new EarsError('NO_SUCH_ENTITY', held.relation
        ? `write(${JSON.stringify(id)}): that is a relation's id, and a relation is no entity. updateRelation(id, patch) changes it; removeRelation(id) removes it`
        : `write(${JSON.stringify(id)}): no entity has this id. create(type) makes one; upsert(type, id) makes it when missing`);
    }
    return builder(id, touch);
  }) as Write;

  // An entity that exists keeps its times: a createdAt or updatedAt in the row is refused, as any write of them
  const upsert = ((type: string, row: Record<string, unknown> & { id: EARS.EntityId }) => {
    requireType(type, 'upsert');
    const { id, shortCode, ...fields } = row;
    refuseShortCode('upsert', type, shortCode);
    // A row bringing its times creates the entity with them, as `create` does; an entity that exists refuses them
    if ((fields.createdAt !== undefined || fields.updatedAt !== undefined) && !exists(id).entity) return create(type, row);
    const now = Date.now();
    const defined = definedFields(fields, 'upsert');
    const result = one({ op: 'upsert', type, id, fields: defined, shortCode }, now) as { created: boolean };
    if (result.created) return builder(id, false);
    return builder(id, true, Object.keys(defined).length ? now : undefined);
  }) as Upsert;

  /**
   * Moves a relation's ends or changes its info, refusing an id no relation has (`NO_SUCH_RELATION`), as
   * `removeRelation` does, and held to the rule `link` is: an end it names must exist when the unit commits
   */
  function updateRelation(relId: EARS.RelationId, { source, target, info }: { source?: EARS.EntityId; target?: EARS.EntityId; info?: unknown }): void {
    one({ op: 'updateRelation', relation: relId, source, target, info });
  }

  /** Removes the relation with the id: refused (`NO_SUCH_RELATION`) when no relation has it */
  function removeRelation(relId: EARS.RelationId): void {
    one({ op: 'removeRelation', relation: relId });
  }

  return { create, upsert, write, updateRelation, removeRelation };
}
