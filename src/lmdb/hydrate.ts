import { EARS } from '../entities.ts';
import type { PartitionPolicy, Partition } from '../persistence/policy.ts';
import type { ShardedPersistence } from '../persistence/sharded-router.ts';
import type { LmdbDbs } from './envs.ts';
import type { EarsAdmin } from '../engine.ts';

const SEP = '\x1F';

function dec(e: { t: string; v: unknown }): unknown {
  if (!e) return null;
  if (e.t === 'date' && typeof e.v === 'string') return new Date(e.v);
  return e.v;
}

/**
 * Loads the partitions `policy` hydrates (and `volatileBackup` with `includeVolatile`) into `engine`,
 * filling `shardedPersistence`'s relation metadata.
 */
export async function hydrateSharded(params: {
  engine: EarsAdmin;
  envs: Record<Partition, LmdbDbs>;
  policy: PartitionPolicy;
  includeVolatile?: boolean;
  shardedPersistence?: ShardedPersistence;
  /** Where the partition counts go; `console.log` by default */
  log?: (message: string) => void;
}) {
  const { engine, envs, policy, includeVolatile = false, shardedPersistence, log = console.log } = params;

  const partitionsToHydrate: Partition[] = [];
  if (policy.hydrate.has('primary')) {
    partitionsToHydrate.push('primary');
  }
  if (includeVolatile || policy.hydrate.has('volatileBackup')) {
    partitionsToHydrate.push('volatileBackup');
  }

  log(`[LMDB] Hydrating partitions: ${partitionsToHydrate.join(', ')}`);

  for (const partition of partitionsToHydrate) {
    const env = envs[partition];

    // Hydrate attributes
    let attrCount = 0;
    for (const { key, value } of env.attrs.getRange()) {
      const keyStr = String(key);
      const sep1 = keyStr.indexOf(SEP);
      const sep2 = keyStr.indexOf(SEP, sep1 + 1);
      const kind = keyStr.substring(0, sep1);
      const entityId = keyStr.substring(sep1 + 1, sep2);

      const idx = Number(keyStr.substring(sep2 + 1));
      engine.bulkLoadAttr(entityId as EARS.EntityId, kind as EARS.AttrKind, dec(value), idx);
      attrCount++;
    }

    // Hydrate relations
    let relCount = 0;
    for (const { key: relId, value: r } of env.relations.getRange()) {
      const relIdStr = String(relId);

      if (!r.src || typeof r.src !== 'string' || r.src.length === 0) {
        console.warn(`[Hydrate] Skipping relation with invalid src: relId=${relIdStr}, src=${r.src}`);
        continue;
      }
      if (!r.tgt || typeof r.tgt !== 'string' || r.tgt.length === 0) {
        console.warn(`[Hydrate] Skipping relation with invalid tgt: relId=${relIdStr}, tgt=${r.tgt}`);
        continue;
      }
      if (!r.kind || typeof r.kind !== 'string') {
        console.warn(`[Hydrate] Skipping relation with invalid kind: relId=${relIdStr}, kind=${r.kind}`);
        continue;
      }

      if (shardedPersistence) {
        shardedPersistence.hydrateRelationMetadata(relIdStr, r.kind, r.src, r.tgt);
      }

      engine.bulkLoadAttr(
        relIdStr as EARS.EntityId,
        EARS.AttrKind.RelationDetails,
        {
          sourceEntity: r.src,
          targetEntity: r.tgt,
          relationType: r.kind,
          info: r.info ?? undefined,
        } satisfies EARS.RelationDetail
      );
      engine.addToIndex(r.kind, r.src, r.tgt, relIdStr as EARS.EntityId);
      relCount++;
    }

    log(`[LMDB] Hydrated ${partition}: ${attrCount} attributes, ${relCount} relations`);
  }
}
