import type { AttributeStorage } from './attribute-storage.ts';
import { createEntity, entityTypeOf } from './attribute-storage.ts';
import type { EdgeStore } from './edge-store.ts';
import type { qx as Qx } from './query.ts';
import type { createGraph } from './graph.ts';
import { EARS } from './entities.ts';
import type { SafeLinkOptions, TransactionBuilder } from './runtime.ts';
import { installedEngine } from './installed.ts';

/** An engine's transactions, over its storage, edge store, queries and graph walks */
export function createTx({ storage, edgeStore, qx, graph, isEntityType: isEntityTypeName }: {
  storage: AttributeStorage;
  edgeStore: EdgeStore;
  qx: typeof Qx;
  graph: Pick<ReturnType<typeof createGraph>, 'wouldCreateCycle' | 'linkSymmetric'>;
  isEntityType: (name: string) => boolean;
}): typeof tx {
  const {
    destroyEntity, entityCreated,
    putAttr, addAttr, mergeAttr, dropAttr, dropIf, updateAttr,
    grantRole, revokeRole,
    addRelation, updateRelation, removeRelationById,
    getRoles,
  } = storage;
  const { wouldCreateCycle, linkSymmetric } = graph;

  function transaction(typeOrId: EARS.Entity | EARS.EntityId, useProvidedId = false): TransactionBuilder {
    const isEntityType = isEntityTypeName(typeOrId);

    const id: EARS.EntityId = isEntityType && !useProvidedId
      ? createEntity(typeOrId as EARS.Entity)
      : (typeOrId as EARS.EntityId);

    // A new entity: `tx(type)` creates one, `tx(id, true)` treats the id as new
    if (isEntityType || useProvidedId) {
      entityCreated(id, isEntityType ? typeOrId as EARS.Entity : undefined);
      // Stamped from the id rather than from `typeOrId`, so both creation paths agree with the index, which
      // is keyed the same way. Without it `tx(Entity.Note).put(...)` produced a row that the engine indexed
      // as a Note and that every feature filtering on the attribute skipped — a row whose id said one thing
      // and whose attributes said nothing. It is written before the caller's puts, so an explicit
      // `.put('entityType', ...)` still wins.
      putAttr(id, EARS.AttrKind.Custom('entityType'), entityTypeOf(id));
      putAttr(id, EARS.AttrKind.Custom('createdAt'), Date.now());
    }

    const preventSelfLoop = (t: EARS.EntityId) => {
      if (t === id) throw new Error("tx.link(): source and target cannot be the same");
    };

    const self = {
      put: (k: EARS.AttrKind | string, v: unknown, allowMultiple = false) => {
        const kind = typeof k === "string" ? EARS.AttrKind.Custom(k) : k;
        if (allowMultiple) {
          addAttr(id, kind, v);
        } else {
          putAttr(id, kind, v);
        }
        return self;
      },
      add: (k: EARS.AttrKind | string, v: unknown) => {
        const kind = typeof k === "string" ? EARS.AttrKind.Custom(k) : k;
        addAttr(id, kind, v);
        return self;
      },
      batchPut: (attrs: Record<string, unknown>) => {
        for (const [k, v] of Object.entries(attrs)) {
          putAttr(id, EARS.AttrKind.Custom(k), v);
        }
        return self;
      },
      merge: (k: EARS.AttrKind, v: unknown, i?: number) => (mergeAttr(id, k, v, i), self),
      drop: (k: EARS.AttrKind, i?: number) => (dropAttr(id, k, i), self),
      dropIf: (k: EARS.AttrKind, c: unknown) => (dropIf(id, k, c), self),
      update: (k: EARS.AttrKind | string, v: unknown) => {
        const kind = typeof k === "string" ? EARS.AttrKind.Custom(k) : k;
        updateAttr(id, kind, v);
        return self;
      },
      updateBatch: (attrs: Record<string, unknown>) => {
        for (const [k, v] of Object.entries(attrs)) {
          updateAttr(id, EARS.AttrKind.Custom(k), v);
        }
        return self;
      },

      grant: (r: string) => {
        if (!getRoles(id).includes(r)) grantRole(id, r);
        return self;
      },
      revoke: (r: string) => {
        if (getRoles(id).includes(r)) revokeRole(id, r);
        return self;
      },
      ensure: (r: string, scope?: readonly EARS.EntityId[]) => {
        const entities = scope ?? qx().withRole(r).ids();
        entities.forEach(e => revokeRole(e, r));
        grantRole(id, r);
        return self;
      },

      link: (k: EARS.RelKind, t: EARS.EntityId, info?: unknown) => {
        preventSelfLoop(t);
        addRelation(id, k, t, info);
        return self;
      },
      relPatch: (
        rel: EARS.EntityId,
        u: { sourceEntity?: EARS.EntityId; targetEntity?: EARS.EntityId; info?: unknown },
      ) => {
        updateRelation(rel, u.sourceEntity, u.targetEntity, u.info);
        return self;
      },
      unlink: (rel: EARS.EntityId) => (removeRelationById(rel), self),

      linkOne: (k: EARS.RelKind, t: EARS.EntityId, info?: unknown) => {
        preventSelfLoop(t);
        edgeStore.linkOne(id, k, t, info);
        return self;
      },
      safeLink: (k: EARS.RelKind, t: EARS.EntityId, options?: SafeLinkOptions) => {
        preventSelfLoop(t);
        const opts = options || {};
        if (opts.acyclicGroup && wouldCreateCycle(id, t, opts.acyclicGroup)) {
          let errorMsg: string;
          if (opts.acyclicGroup.length === 1) {
            errorMsg = `Cannot create a ${k} relation that would form a cycle`;
          } else {
            const kinds = opts.acyclicGroup.join(', ');
            errorMsg = `Cannot create a ${k} relation that would form a cycle within [${kinds}]`;
          }
          throw new Error(errorMsg);
        }
        if (opts.symmetric) {
          linkSymmetric(id, t, k, opts.info);
          return self;
        }
        return self.linkOne(k, t, opts.info);
      },
      patchLink: (
        k: EARS.RelKind,
        t: EARS.EntityId,
        u: { newTarget: EARS.EntityId; newInfo?: unknown },
      ) => {
        edgeStore.patchOne(
          { sourceEntity: id, relationType: k, targetEntity: t },
          { newTarget: u.newTarget, newInfo: u.newInfo },
        );
        return self;
      },
      unlinkIf: (k: EARS.RelKind, t?: EARS.EntityId) => (
        edgeStore.unlink({ sourceEntity: id, relationType: k, targetEntity: t }),
        self
      ),
      unlinkWhere: (c?: { kind?: EARS.RelKind; target?: EARS.EntityId }) => (
        edgeStore.unlink({
          sourceEntity: id,
          targetEntity: c?.target,
          relationType: c?.kind,
        }),
        self
      ),
      define: (def: {
        attributes?: Record<string, unknown>;
        links?: [EARS.RelKind, EARS.EntityId] | Array<[EARS.RelKind, EARS.EntityId]>;
        roles?: string | string[];
      }) => {
        if (def.attributes) {
          self.batchPut(def.attributes);
        }
        if (def.links) {
          const links = Array.isArray(def.links[0])
            ? def.links as Array<[EARS.RelKind, EARS.EntityId]>
            : [def.links] as Array<[EARS.RelKind, EARS.EntityId]>;
          for (const [kind, target] of links) {
            self.link(kind, target);
          }
        }
        if (def.roles) {
          const roles = Array.isArray(def.roles) ? def.roles : [def.roles];
          for (const role of roles) {
            self.grant(role);
          }
        }
        return self;
      },

      destroy: (skipPersistence = false) => (destroyEntity(id, skipPersistence), undefined as never),

      id: () => id,
    } as const;

    return self;
  }

  return transaction;
}

export function tx(typeOrId: EARS.Entity | EARS.EntityId, useProvidedId = false): TransactionBuilder {
  return installedEngine().tx(typeOrId, useProvidedId);
}
