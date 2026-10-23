// The engine's writes. Each entry point makes one claim about its argument and is held to it: `create` names a
// type, `write` an entity that exists, and `upsert` a type and an id that is either free or that type's.
import type { AttributeStorage } from './attribute-storage.ts';
import type { Journal } from './journal.ts';
import { createEntity, entityTypeOf } from './attribute-storage.ts';
import type { EdgeStore } from './edge-store.ts';
import type { createGraph } from './graph.ts';
import { EARS } from './entities.ts';
import { EarsError, refuseReserved } from './errors.ts';
import type { CreateOptions, LinkOptions, WriteBuilder } from './runtime.ts';

const LINK_OPTIONS: readonly string[] = ['info', 'replace', 'acyclicIn', 'symmetric'] satisfies Array<keyof LinkOptions>;

/** A string that is not shaped like an id: where a type is meant, an id fails to compile */
export type TypeName<T extends string> = T extends `${string}-${string}` ? never : T;

/** Creates an entity of a registered type, with a new id or the one given */
export interface Create {
  <E extends string>(type: TypeName<E>, options?: CreateOptions): WriteBuilder<E>;
}

/** Writes to the entity with this id, creating it as `type` when no entity has it */
export interface Upsert {
  <E extends string>(type: TypeName<E>, id: EARS.EntityId): WriteBuilder<E>;
}

/** Writes to the entity with this id, which must exist */
export interface Write {
  <E extends string>(id: EARS.EntityId<E>): WriteBuilder<E>;
}

/** An engine's writes, over its storage, edge store and graph walks */
export function createWrites({ storage, edgeStore, graph, isEntityType, journal }: {
  storage: AttributeStorage;
  journal: Pick<Journal, 'atCommit' | 'open'>;
  edgeStore: EdgeStore;
  graph: Pick<ReturnType<typeof createGraph>, 'wouldCreateCycle' | 'linkSymmetric'>;
  isEntityType: (name: string) => boolean;
}) {
  const {
    destroyEntity, entityCreated, hasEntity,
    putAttr, appendAttr, mergeAttr, removeAt, removeValue, deleteAttr,
    grantRole, revokeRole, getRoles, addRelation, findEntitiesByRelationTo, getAttr,
  } = storage;

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
      if (!hasEntity(target) && findEntitiesByRelationTo(kind, target).includes(source)) throw missing();
    });
  }
  const { wouldCreateCycle, linkSymmetric } = graph;
  const attr = (k: string) => EARS.AttrKind.Custom(k);

  function requireType(type: string, via: string): void {
    if (typeof type === 'string' && isEntityType(type)) return;
    throw new EarsError(
      'UNKNOWN_ENTITY_TYPE',
      `${via}(${JSON.stringify(type)}): not a registered entity type.` +
        (typeof type === 'string' && type.includes('-') ? ' It looks like an id: use write(id).' : ''),
    );
  }

  function builder(id: EARS.EntityId): WriteBuilder {
    const self: WriteBuilder = {
      set(k, v) {
        refuseReserved(k, 'set');
        putAttr(id, attr(k), v);
        return self;
      },
      setMany(attrs) {
        for (const k of Object.keys(attrs)) refuseReserved(k, 'setMany');
        for (const [k, v] of Object.entries(attrs)) putAttr(id, attr(k), v);
        return self;
      },
      add(k, v) {
        refuseReserved(k, 'add');
        appendAttr(id, attr(k), v);
        return self;
      },
      merge(k, v) {
        refuseReserved(k, 'merge');
        mergeAttr(id, attr(k), v);
        return self;
      },
      delete(k) {
        refuseReserved(k, 'delete');
        deleteAttr(id, attr(k));
        return self;
      },
      deleteAt(k, index) {
        refuseReserved(k, 'deleteAt');
        removeAt(id, attr(k), index);
        return self;
      },
      deleteValue(k, v) {
        refuseReserved(k, 'deleteValue');
        removeValue(id, attr(k), v);
        return self;
      },

      link(kind, target, options: LinkOptions = {}) {
        const { info, replace, acyclicIn, symmetric } = options;
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
        if (symmetric) linkSymmetric(id, target, kind, info);
        else if (replace) edgeStore.linkOne(id, kind, target, info);
        else addRelation(id, kind, target, info);
        return self;
      },
      unlink(kind, target) {
        edgeStore.unlink({ sourceEntity: id, relationType: kind, targetEntity: target });
        return self;
      },
      unlinkAll() {
        edgeStore.unlink({ sourceEntity: id });
        return self;
      },

      grant(role) {
        if (!getRoles(id).includes(role)) grantRole(id, role);
        return self;
      },
      revoke(role) {
        if (getRoles(id).includes(role)) revokeRole(id, role);
        return self;
      },
      grantExclusive(role, among) {
        for (const other of among) {
          if (other !== id && getRoles(other).includes(role)) revokeRole(other, role);
        }
        return self.grant(role);
      },

      destroy({ keepStored = false } = {}) {
        destroyEntity(id, keepStored);
      },

      id: () => id,
    };
    return self;
  }

  const create = ((type: string, { id: given, createdAt }: CreateOptions = {}) => {
    requireType(type, 'create');
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
      id = createEntity(type);
    }
    entityCreated(id, type);
    putAttr(id, attr('createdAt'), createdAt ?? Date.now());
    return builder(id);
  }) as Create;

  const write = ((id: EARS.EntityId) => {
    if (!hasEntity(id)) {
      throw new EarsError('NO_SUCH_ENTITY', `write(${JSON.stringify(id)}): no entity has this id. create(type) makes one; upsert(type, id) makes it when missing`);
    }
    return builder(id);
  }) as Write;

  const upsert = ((type: string, id: EARS.EntityId) => {
    requireType(type, 'upsert');
    if (!hasEntity(id)) return create(type, { id });
    if (entityTypeOf(id) !== type) {
      throw new EarsError('TYPE_MISMATCH', `upsert(${JSON.stringify(type)}, ${JSON.stringify(id)}): that entity is a ${entityTypeOf(id)}`);
    }
    return builder(id);
  }) as Upsert;

  /**
   * Moves a relation's ends or changes its info, held to the rule `link` is: an end it names must exist — at once
   * outside a unit, before anything is written, and when the unit commits inside one, where it reads the relation's
   * ends as they are then. `admin.updateRelation` stays unchecked, as `admin.addRelation` does: loading data is the
   * creator's to vouch for.
   */
  function updateRelation(relId: EARS.EntityId, { sourceEntity, targetEntity, info }: { sourceEntity?: EARS.EntityId; targetEntity?: EARS.EntityId; info?: unknown }): void {
    const missing = (end: EARS.EntityId) => new EarsError('NO_SUCH_ENTITY', `updateRelation(${JSON.stringify(relId)}): no entity has the id ${JSON.stringify(end)}`);
    if (!journal.open) {
      for (const end of [sourceEntity, targetEntity]) if (end !== undefined && !hasEntity(end)) throw missing(end);
    } else {
      journal.atCommit(() => {
        const details = getAttr(relId, EARS.AttrKind.RelationDetails) as EARS.RelationDetail | null;
        if (!details) return;
        for (const end of [details.sourceEntity, details.targetEntity]) if (!hasEntity(end)) throw missing(end);
      });
    }
    storage.updateRelation(relId, sourceEntity, targetEntity, info);
  }

  return { create, upsert, write, updateRelation };
}
