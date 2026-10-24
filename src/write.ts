// The engine's writes. Each entry point makes one claim about its argument and is held to it: `create` names a
// type, `write` an entity that exists, and `upsert` a type and an id that is either free or that type's.
import type { AttributeStorage } from './attribute-storage.ts';
import type { Journal } from './journal.ts';
import { newId, entityTypeOf, TRASHED_AT } from './attribute-storage.ts';
import type { Relations } from './relations.ts';
import type { createHierarchy } from './hierarchy.ts';
import type { createEntityCounters } from './numbering.ts';
import { EARS } from './entities.ts';
import { EarsError, RESERVED_ENTITY_TYPES, refuseReserved } from './errors.ts';
import type { NameChecks } from './names.ts';
import type { StoreAccess } from './store-access.ts';
import type { LinkOptions, RowIdentity, RowRules, WriteBuilder, WriteOptions, WriteTarget } from './runtime.ts';

/** When the entity last changed: stamped by the engine, never written by a caller */
const UPDATED_AT = EARS.AttrKind.Custom('updatedAt');

const LINK_OPTIONS: readonly string[] = ['info', 'replace', 'acyclicIn'] satisfies Array<keyof LinkOptions>;

/** The write builder's members that write, each run as one unit */
const UNIT_WRITES = [
  'set', 'setMany', 'append', 'merge', 'delete', 'removeAt', 'removeValue', 'link', 'unlink', 'unlinkAll',
  'grant', 'revoke', 'grantExclusive', 'trash', 'restore', 'destroy',
] as const satisfies ReadonlyArray<keyof WriteBuilder>;

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

/** An engine's writes, over its attribute storage, its relations and its cycle check */
export function createWrites({ storage, relations, hierarchy, isEntityType, rowRules, counters, names, journal, access }: {
  storage: AttributeStorage;
  /** The store, which each write runs in a unit of: a write that fails part way keeps nothing */
  access: Pick<StoreAccess, 'write'>;
  journal: Pick<Journal, 'atCommit' | 'open'>;
  relations: Relations;
  hierarchy: Pick<ReturnType<typeof createHierarchy>, 'wouldCreateCycle'>;
  isEntityType: (name: string) => boolean;
  rowRules: (type: string) => RowRules | undefined;
  counters: Pick<ReturnType<typeof createEntityCounters>, 'generateShortCode'>;
  names: NameChecks;
}) {
  const requiredFields = (type: string) => rowRules(type)?.required ?? [];
  const {
    destroyEntity, entityCreated, hasEntity, isTrashed,
    putAttr, appendAttr, mergeAttr, removeAt, removeValue, deleteAttr,
    grantRole, revokeRole, getRoles, getRow, getAttr, holders,
  } = storage;

  const SHORT_CODE = EARS.AttrKind.Custom('shortCode');

  /**
   * The short code a created row of `type` gets: the one it brought — an import keeping the code it had, held to the
   * type's `<prefix>-<n>` and to no other entity holding it — or, with none, the next number of the type's prefix.
   * Undefined for a type that numbers none, which a row may then bring no code to either.
   */
  function shortCodeFor(type: string, brought: unknown): (() => string) | undefined {
    const prefix = rowRules(type)?.shortCode;
    if (brought === undefined) return prefix ? () => counters.generateShortCode(type, prefix) : undefined;
    if (!prefix) {
      throw new EarsError('INVALID_SHORT_CODE', `create(${JSON.stringify(type)}, { shortCode: ${JSON.stringify(brought)} }): a ${type} is not numbered, so it holds no short code`);
    }
    if (typeof brought !== 'string' || !new RegExp(`^${prefix}-[1-9][0-9]*$`).test(brought)) {
      throw new EarsError('INVALID_SHORT_CODE', `create(${JSON.stringify(type)}, { shortCode: ${JSON.stringify(brought)} }): a ${type}'s short code is ${prefix}-<n>`);
    }
    if (holders(type as EARS.Entity, SHORT_CODE, brought).length > 0) {
      throw new EarsError('SHORT_CODE_IN_USE', `create(${JSON.stringify(type)}, { shortCode: ${JSON.stringify(brought)} }): another entity holds this short code`);
    }
    return () => brought;
  }

  /**
   * A relation has two ends when it is committed. Inside a unit that is checked when the unit commits, so the unit
   * may link to an entity it creates later — a forward reference — and a link whose target it destroyed meanwhile
   * is gone with it, since destroying an entity removes its relations. Outside a unit a write is committed as it
   * is made, so the target must already be there.
   */
  function requireTarget(source: EARS.EntityId, kind: string, target: EARS.EntityId): void {
    const missing = () => new EarsError('NO_SUCH_ENTITY', `link(${kind}, ${JSON.stringify(target)}): no entity has this id`);
    if (!journal.open) {
      if (!hasEntity(target)) throw missing();
      return;
    }
    journal.atCommit(() => {
      // This link's own edge: another entity's relation to the same missing id, loaded from files, is not this unit's
      if (!hasEntity(target) && relations.farEnds(kind, target, false).includes(source)) throw missing();
    });
  }
  const { wouldCreateCycle } = hierarchy;
  const attr = (k: string) => EARS.AttrKind.Custom(k);

  function requireType(type: string, via: string): void {
    // The engine's own, whatever its creator registers: a row of one would stand where a relation or a numbering
    // sequence is kept
    if ((Object.values(RESERVED_ENTITY_TYPES) as string[]).includes(type)) {
      throw new EarsError('RESERVED_ENTITY_TYPE', `${via}(${JSON.stringify(type)}): the engine's own type, which its ids are written with: no entity is created as one`);
    }
    if (typeof type === 'string' && isEntityType(type)) return;
    throw new EarsError(
      'UNKNOWN_ENTITY_TYPE',
      `${via}(${JSON.stringify(type)}): not a registered entity type.` +
        (typeof type === 'string' && type.includes('-') ? ' It looks like an id: use write(id).' : ''),
    );
  }

  /**
   * The writes to one entity. Its attribute writes stamp `updatedAt` — once per builder, so a chain of them is one
   * change — unless `touch` is off; links, roles and the trash don't.
   */
  function builder(id: EARS.EntityId, touch = true): WriteBuilder {
    let stamped = !touch;
    const changed = () => {
      if (stamped) return;
      stamped = true;
      putAttr(id, UPDATED_AT, Date.now());
    };
    const self: WriteBuilder = {
      set(k, v) {
        refuseReserved(k, 'set');
        if (v === undefined) throw new EarsError('NO_VALUE', `set('${k}'): no value to store. delete('${k}') removes the attribute`);
        putAttr(id, attr(k), v);
        changed();
        return self;
      },
      setMany(attrs) {
        for (const k of Object.keys(attrs)) refuseReserved(k, 'setMany');
        const defined = Object.entries(attrs).filter(([, v]) => v !== undefined);
        for (const [k, v] of defined) putAttr(id, attr(k), v);
        if (defined.length > 0) changed();
        return self;
      },
      append(k, v) {
        refuseReserved(k, 'append');
        appendAttr(id, attr(k), v);
        changed();
        return self;
      },
      merge(k, v) {
        refuseReserved(k, 'merge');
        mergeAttr(id, attr(k), v);
        changed();
        return self;
      },
      delete(k) {
        refuseReserved(k, 'delete');
        if (requiredFields(entityTypeOf(id)).includes(k)) {
          throw new EarsError('REQUIRED_FIELD', `delete(${JSON.stringify(k)}): every ${entityTypeOf(id)} holds it; set(${JSON.stringify(k)}, value) replaces it`);
        }
        deleteAttr(id, attr(k));
        changed();
        return self;
      },
      removeAt(k, index) {
        refuseReserved(k, 'removeAt');
        removeAt(id, attr(k), index);
        changed();
        return self;
      },
      removeValue(k, v) {
        refuseReserved(k, 'removeValue');
        removeValue(id, attr(k), v);
        changed();
        return self;
      },

      link(kind, target, options: LinkOptions = {}) {
        const { info, replace, acyclicIn } = options;
        names.relationKinds(kind, 'link');
        if (acyclicIn) names.relationKinds(acyclicIn, 'link');
        // An edge's info is `{ info }`: an object with other keys is info passed where the options go
        const stray = Object.keys(options).filter((k) => !LINK_OPTIONS.includes(k));
        if (stray.length) throw new Error(`link(${kind}): unknown option ${stray.join(', ')}; an edge's info goes in { info }`);
        if (target === id) throw new Error(`link(${kind}): an entity can't be linked to itself (${id})`);
        if (acyclicIn && wouldCreateCycle(id, target, acyclicIn)) {
          throw new Error(acyclicIn.length === 1
            ? `Cannot create a ${kind} relation that would form a cycle`
            : `Cannot create a ${kind} relation that would form a cycle within [${acyclicIn.join(', ')}]`);
        }
        requireTarget(id, kind, target);
        if (replace) relations.replace(id, kind, target, info);
        else relations.add(id, kind, target, info);
        return self;
      },
      unlink(kind, target) {
        names.relationKinds(kind, 'unlink');
        relations.unlink({ source: id, kind, target });
        return self;
      },
      unlinkAll() {
        relations.unlink({ source: id });
        return self;
      },

      grant(role) {
        names.role(role, 'grant');
        if (!getRoles(id).includes(role)) grantRole(id, role);
        return self;
      },
      revoke(role) {
        names.role(role, 'revoke');
        if (getRoles(id).includes(role)) revokeRole(id, role);
        return self;
      },
      grantExclusive(role, among) {
        names.role(role, 'grantExclusive');
        for (const other of among) {
          if (other !== id && getRoles(other).includes(role)) revokeRole(other, role);
        }
        return self.grant(role);
      },

      trash(at = Date.now()) {
        if (!isTrashed(id)) putAttr(id, TRASHED_AT, at);
        return self;
      },
      restore() {
        deleteAttr(id, TRASHED_AT);
        return self;
      },

      destroy() {
        relations.removeEntity(id);
        destroyEntity(id);
      },

      id: () => id,

      // The builder's entity exists — write refuses an id no entity has, and destroy ends the chain
      row: () => getRow(id) as ReturnType<WriteBuilder['row']>,
    };
    // Each write is one unit: one that writes several keys, or several attributes, keeps none of them when it fails
    for (const name of UNIT_WRITES) {
      const method = self[name] as (...args: unknown[]) => unknown;
      (self as unknown as Record<string, unknown>)[name] = (...args: unknown[]) => access.write(() => method(...args));
    }
    return self;
  }

  const create = ((type: string, row: Record<string, unknown> & RowIdentity = {}) => access.write(() => {
    requireType(type, 'create');
    // The row's id, short code and times are the engine's to write; its other reserved names are refused before the
    // entity exists, so a create that fails leaves nothing behind
    const { id: given, shortCode: brought, createdAt, updatedAt, ...fields } = row;
    for (const field of Object.keys(fields)) refuseReserved(field, 'create');
    // Numbered once every check has passed, so a refused create takes no number
    const shortCode = shortCodeFor(type, brought);
    const missing = requiredFields(type).filter((field) => fields[field] === undefined);
    if (missing.length > 0) {
      throw new EarsError('REQUIRED_FIELD', `create(${JSON.stringify(type)}): the row lacks ${missing.map((field) => `"${field}"`).join(', ')}, which every ${type} holds`);
    }
    let id: EARS.EntityId;
    if (given !== undefined) {
      // An id is the record of its entity's type (`entityTypeOf`), so it must name the type it is created as
      if (entityTypeOf(given) !== type) {
        throw new EarsError('TYPE_MISMATCH', `create(${JSON.stringify(type)}, { id: ${JSON.stringify(given)} }): the id names another type`);
      }
      if (hasEntity(given)) {
        throw new EarsError('ID_IN_USE', `create(${JSON.stringify(type)}, { id: ${JSON.stringify(given)} }): an entity already has this id; write(id) changes it, upsert(type, id) does either`);
      }
      id = given;
    } else {
      id = newId(type);
    }
    entityCreated(id, type);
    if (shortCode) putAttr(id, SHORT_CODE, shortCode());
    const at = createdAt ?? Date.now();
    putAttr(id, attr('createdAt'), at);
    putAttr(id, UPDATED_AT, updatedAt ?? at);
    // The creation is the entity's first change: its fields, and what the builder writes now, are part of it
    return builder(id, false).setMany(fields);
  })) as Create;

  const write = ((id: EARS.EntityId, { touch = true }: WriteOptions = {}) => {
    if (!hasEntity(id)) {
      // A relation's id reaches here only untyped (the compiler refuses one), so the refusal names where it goes
      throw new EarsError('NO_SUCH_ENTITY', relations.has(id)
        ? `write(${JSON.stringify(id)}): that is a relation's id, and a relation is no entity. updateRelation(id, patch) changes it; removeRelation(id) removes it`
        : `write(${JSON.stringify(id)}): no entity has this id. create(type) makes one; upsert(type, id) makes it when missing`);
    }
    return builder(id, touch);
  }) as Write;

  // An entity that exists keeps its times: setMany refuses a createdAt or updatedAt in the row, as any write of them
  const upsert = ((type: string, { id, shortCode, ...fields }: Record<string, unknown> & { id: EARS.EntityId }) => access.write(() => {
    requireType(type, 'upsert');
    if (!hasEntity(id)) return create(type, { ...fields, ...(shortCode !== undefined && { shortCode }), id });
    if (entityTypeOf(id) !== type) {
      throw new EarsError('TYPE_MISMATCH', `upsert(${JSON.stringify(type)}, { id: ${JSON.stringify(id)} }): that entity is a ${entityTypeOf(id)}`);
    }
    // An entity keeps the short code it was numbered with: a row may repeat it, as an import's does, and not change it
    if (shortCode !== undefined && shortCode !== getAttr(id, SHORT_CODE)) {
      throw new EarsError('INVALID_SHORT_CODE', `upsert(${JSON.stringify(type)}, { id: ${JSON.stringify(id)}, shortCode: ${JSON.stringify(shortCode)} }): the entity's short code is ${JSON.stringify(getAttr(id, SHORT_CODE))}, and nothing changes it`);
    }
    return builder(id, true).setMany(fields);
  })) as Upsert;

  /**
   * Moves a relation's ends or changes its info, refusing an id no relation has (`NO_SUCH_RELATION`), as
   * `removeRelation` does, and held to the rule `link` is: an end it names must exist — at once
   * outside a unit, before anything is written, and when the unit commits inside one, where it reads the relation's
   * ends as they are then. `admin.updateRelation` stays unchecked, as `admin.addRelation` does: loading data is the
   * creator's to vouch for.
   */
  function updateRelation(relId: EARS.RelationId, { source, target, info }: { source?: EARS.EntityId; target?: EARS.EntityId; info?: unknown }): void {
    if (!relations.has(relId)) {
      throw new EarsError('NO_SUCH_RELATION', `updateRelation(${JSON.stringify(relId)}): no relation has this id`);
    }
    const missing = (end: EARS.EntityId) => new EarsError('NO_SUCH_ENTITY', `updateRelation(${JSON.stringify(relId)}): no entity has the id ${JSON.stringify(end)}`);
    if (!journal.open) {
      for (const end of [source, target]) if (end !== undefined && !hasEntity(end)) throw missing(end);
    } else {
      journal.atCommit(() => {
        const details = relations.get(relId);
        if (!details) return;
        for (const end of [details.source, details.target]) if (!hasEntity(end)) throw missing(end);
      });
    }
    relations.update(relId, source, target, info);
  }

  /** Removes the relation with the id: refused (`NO_SUCH_RELATION`) when no relation has it */
  function removeRelation(relId: EARS.RelationId): void {
    if (!relations.has(relId)) {
      throw new EarsError('NO_SUCH_RELATION', `removeRelation(${JSON.stringify(relId)}): no relation has this id`);
    }
    relations.remove(relId);
  }

  return { create, upsert, write, updateRelation, removeRelation };
}
