import { EARS } from '../entities.ts';
import type { PersistenceSink } from '../runtime.ts';
import type { PartitionPolicy, Partition } from './policy.ts';

/** A sink routing each write to the sink of its partition, and the routing caches hydration fills */
export interface ShardedPersistence extends PersistenceSink {
  hydrateRelationMetadata(relId: string, kind: string, src: string, tgt: string): void;
  getRelMeta(): Map<string, { kind: string; src: string; tgt: string }>;
}

const entTypeOf = (id: string): EARS.Entity => {
  if (typeof id !== 'string' || id.length === 0) {
    throw new Error(`[Sharded] Invalid entity id for routing: ${JSON.stringify(id)} (${typeof id})`);
  }
  const dash = id.indexOf('-');
  return (dash === -1 ? id : id.slice(0, dash)) as EARS.Entity;
};

/** A relation's details as the engine holds them, or null (`admin`'s relation details reader) */
export type RelationDetailsReader = (relId: EARS.EntityId) => EARS.RelationDetail | null;

/**
 * Routes sink calls to `sinks` by the partition `policy` gives each entity and relation. A relation it
 * hasn't seen is routed by its details in the engine, read with `relationDetails` (none by default).
 */
export function makeShardedPersistence(
  policy: PartitionPolicy,
  sinks: Record<Partition, PersistenceSink>,
  relationDetails: RelationDetailsReader = () => null,
): ShardedPersistence {
  // Each relation's kind and ends as last written, which decide its partition
  const relMeta = new Map<string, { kind: string; src: string; tgt: string }>();

  // A relation the router didn't see written (after a reopen): the engine's details, already updated
  function getRelationMeta(relId: string) {
    const d = relationDetails(relId as EARS.EntityId);
    return d ? {
      kind: d.relationType,
      src: d.sourceEntity as string,
      tgt: d.targetEntity as string
    } : null;
  }

  const pickEntity = (entityId: string, entityType?: EARS.Entity) =>
    policy.routeEntity(entityId, entityType);

  const pickRel = (src: string, tgt: string) =>
    policy.routeRelation({ srcType: entTypeOf(src), tgtType: entTypeOf(tgt) });

  // Track relation locations for efficient updates
  const relationPartitions = new Map<string, Partition>();

  // Helper to compute partition for a relation
  function computePartitionFor(
    relId: string,
    metaHint?: { kind: string; src: string; tgt: string }
  ): Partition | null {
    const meta = metaHint ?? relMeta.get(relId) ?? getRelationMeta(relId);
    if (!meta) return null;

    return policy.routeRelation({
      srcType: entTypeOf(meta.src),
      tgtType: entTypeOf(meta.tgt),
    });
  }

  return {
    onCreateEntity(entityId: string, entityType?: string) {
      const p = pickEntity(entityId, entityType as EARS.Entity);
      sinks[p].onCreateEntity(entityId, entityType);
    },

    onDestroyEntity(entityId: string) {
      // Destruction is semantic; delete from whichever partition it lives in
      const p = pickEntity(entityId);
      // Its relations were removed before this, each with onRemoveRelation
      sinks[p].onDestroyEntity(entityId);
    },

    onPutAttrArray(kind: string, entityId: string, values: unknown[]) {
      sinks[pickEntity(entityId)].onPutAttrArray(kind, entityId, values);
    },

    onDropAttr(kind: string, entityId: string, idx: number, entireArray?: unknown[]) {
      // Enforce entireArray requirement for consistency
      if (!entireArray) {
        throw new Error('[Sharded] onDropAttr requires entireArray parameter to avoid index drift');
      }
      const p = pickEntity(entityId);
      sinks[p].onDropAttr(kind, entityId, idx, entireArray);
    },

    onAddRelation(relId: string, kind: string, src: string, tgt: string, info: unknown) {
      const p = pickRel(src, tgt);

      // Update caches
      relMeta.set(relId, { kind, src, tgt });
      relationPartitions.set(relId, p);

      sinks[p].onAddRelation(relId, kind, src, tgt, info);
    },

    onUpdateRelation(relId: string, patch: { src?: string; tgt?: string; info?: unknown }) {
      const prev = relMeta.get(relId);

      if (!prev) {
        // Not seen by this sink (a reopened store's, say). The engine has already applied the update, so its
        // details are the relation's new ends, and the partition it was in is unknown: write it to its new
        // partition and remove it from the others
        const current = getRelationMeta(relId);
        if (!current) {
          console.warn('[Sharded] onUpdateRelation called with unknown relId:', relId);
          // Best effort: try all partitions
          for (const sink of Object.values(sinks)) {
            sink.onUpdateRelation(relId, patch);
          }
          return;
        }
        const p = computePartitionFor(relId, current)!;
        for (const [partition, sink] of Object.entries(sinks)) {
          if (partition !== p) sink.onRemoveRelation(relId);
        }
        sinks[p].onAddRelation(relId, current.kind, current.src, current.tgt, relationDetails(relId as EARS.EntityId)?.info);
        relMeta.set(relId, current);
        relationPartitions.set(relId, p);
        return;
      }

      // Get previous info if available (for preserving during moves)
      const prevDetails = relationDetails(relId as EARS.EntityId);
      const prevInfo = prevDetails?.info;

      // Build next metadata - use presence checks with validation
      const next = { ...prev };
      if ('src' in patch) {
        if (patch.src == null || patch.src === '') throw new Error('[Sharded] patch.src is empty');
        next.src = patch.src as string;
      }
      if ('tgt' in patch) {
        if (patch.tgt == null || patch.tgt === '') throw new Error('[Sharded] patch.tgt is empty');
        next.tgt = patch.tgt as string;
      }

      // Compute current and new partitions
      const curP = computePartitionFor(relId, prev);
      const newP = computePartitionFor(relId, next);

      if (!curP || !newP) {
        console.error('[Sharded] Failed to compute partition for relation:', relId);
        return;
      }

      if (curP !== newP) {
        // Relation needs to move partitions - preserve info if not in patch
        sinks[curP].onRemoveRelation(relId);
        sinks[newP].onAddRelation(
          relId,
          next.kind,
          next.src,
          next.tgt,
          'info' in patch ? patch.info : prevInfo  // Preserve info when moving
        );
        relationPartitions.set(relId, newP);
      } else {
        // Same partition, just update
        sinks[curP].onUpdateRelation(relId, patch);
      }

      // Update cache
      relMeta.set(relId, next);
    },

    onRemoveRelation(relId: string, destroyed?: string) {
      const p = relationPartitions.get(relId) ?? computePartitionFor(relId);

      if (p) {
        // Keep the stored link when an entity is being deleted and the link is saved in a different partition
        // from that entity: the link then belongs to its other entity. Example: a run record (TNode, in the trace
        // partition) has a link to the node it ran (in the main partition). Deleting the node must not erase the
        // run's history, so the link stays in the trace partition. Deleting the run, or unlinking, still removes it.
        const keepAsHistory = destroyed !== undefined && pickEntity(destroyed) !== p;
        if (!keepAsHistory) sinks[p].onRemoveRelation(relId);
        relationPartitions.delete(relId);
        relMeta.delete(relId);
      } else if (destroyed !== undefined) {
        // Partition unknown (the router didn't see this link written, e.g. after a store reopen): remove it only
        // from the deleted entity's partition, so a history link saved in the other partition stays (as above)
        sinks[pickEntity(destroyed)].onRemoveRelation(relId);
      } else {
        // Partition unknown and the link itself was removed: remove it from every partition
        for (const sink of Object.values(sinks)) {
          sink.onRemoveRelation(relId);
        }
        relMeta.delete(relId);
      }
    },

    close() {
      // Close all sinks, not just hardcoded ones
      for (const sink of Object.values(sinks)) {
        sink.close?.();
      }
    },

    getErrorStats() {
      let errorCount = 0;
      let lastError: unknown = null;

      // Aggregate stats from all sinks
      for (const sink of Object.values(sinks)) {
        const stats = sink.getErrorStats?.();
        if (stats) {
          errorCount += stats.errorCount ?? 0;
          lastError = lastError ?? stats.lastError;
        }
      }

      return { errorCount, lastError };
    },

    // What hydration read off disk, recorded so a relation routes to the partition it is already in
    hydrateRelationMetadata(relId: string, kind: string, src: string, tgt: string) {
      // Validate inputs to prevent entTypeOf from throwing
      if (!src || typeof src !== 'string' || src.length === 0) {
        console.warn(`[Sharded] Invalid src in hydrateRelationMetadata: relId=${relId}, src=${src}`);
        return;
      }
      if (!tgt || typeof tgt !== 'string' || tgt.length === 0) {
        console.warn(`[Sharded] Invalid tgt in hydrateRelationMetadata: relId=${relId}, tgt=${tgt}`);
        return;
      }

      relMeta.set(relId, { kind, src, tgt });
      relationPartitions.set(
        relId,
        policy.routeRelation({
          srcType: entTypeOf(src),
          tgtType: entTypeOf(tgt)
        })
      );
    },

    // Expose read-only copy for testing/debugging
    getRelMeta() {
      // Return a read-only copy to prevent external mutations
      return new Map(relMeta);
    }
  };
}
