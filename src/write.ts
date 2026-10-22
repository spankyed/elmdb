// The engine's writes. Each entry point makes one claim about its argument and is held to it: `create` names a
// type, `write` an entity that exists, and `upsert` a type and an id that is either free or that type's.
import type { AttributeStorage } from './attribute-storage.ts';
import { createEntity, entityTypeOf } from './attribute-storage.ts';
import type { EdgeStore } from './edge-store.ts';
import type { createGraph } from './graph.ts';
import { EARS } from './entities.ts';
import { EarsError, refuseReserved } from './errors.ts';
import type { CreateOptions, LinkOptions, WriteBuilder } from './runtime.ts';
import { installedEngine } from './installed.ts';

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
export function createWrites({ storage, edgeStore, graph, isEntityType }: {
  storage: AttributeStorage;
  edgeStore: EdgeStore;
  graph: Pick<ReturnType<typeof createGraph>, 'wouldCreateCycle' | 'linkSymmetric'>;
  isEntityType: (name: string) => boolean;
}) {
  const {
    destroyEntity, entityCreated, hasEntity,
    putAttr, addAttr, mergeAttr, dropAttr, deleteAttr, dropIf,
    grantRole, revokeRole, getRoles, addRelation,
  } = storage;
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
        addAttr(id, attr(k), v);
        return self;
      },
      merge(k, v, index) {
        refuseReserved(k, 'merge');
        mergeAttr(id, attr(k), v, index);
        return self;
      },
      delete(k) {
        refuseReserved(k, 'delete');
        deleteAttr(id, attr(k));
        return self;
      },
      deleteAt(k, index) {
        refuseReserved(k, 'deleteAt');
        dropAttr(id, attr(k), index);
        return self;
      },
      deleteValue(k, v) {
        refuseReserved(k, 'deleteValue');
        dropIf(id, attr(k), v);
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

  return { create, upsert, write };
}

// The installed engine's writes: the untyped twins of a pack's typed `create`, `upsert` and `write`

/** `create` on the installed engine, unchecked against any pack's shapes */
export const untypedCreate: Create = ((type: string, options?: CreateOptions) =>
  installedEngine().create(type as never, options)) as unknown as Create;

/** `upsert` on the installed engine, unchecked against any pack's shapes */
export const untypedUpsert: Upsert = ((type: string, id: EARS.EntityId) =>
  installedEngine().upsert(type as never, id)) as unknown as Upsert;

/** `write` on the installed engine, unchecked against any pack's shapes */
export const untypedWrite: Write = ((id: EARS.EntityId) => installedEngine().write(id)) as Write;

/** Moves a relation to new ends, or replaces its info, keeping its id, on the installed engine */
export function updateRelation(relId: EARS.EntityId, patch: { sourceEntity?: EARS.EntityId; targetEntity?: EARS.EntityId; info?: unknown }): void {
  installedEngine().updateRelation(relId, patch);
}

/** Runs `fn` as one unit on the installed engine (`EarsQuery.transact`) */
export function transact<T>(fn: () => T): T {
  return installedEngine().transact(fn);
}
