import type { EARS } from '../entities.ts';

/** Where a persisted entity or relation lives: the store the app hydrates, or the volatile store beside it */
export type Partition = 'primary' | 'volatileBackup';

export interface PartitionPolicy {
  /** Which partition an entity of this type lives in */
  routeEntity(entityType: EARS.Entity): Partition;

  /** Which partition a relation lives in */
  routeRelation(params: { srcType: EARS.Entity; tgtType: EARS.Entity }): Partition;

  /** The partitions hydrated at startup (by default only primary) */
  readonly hydrate: ReadonlySet<Partition>;
}

/**
 * Routes the entity types in `excludedEntityTypes`, and relations that touch one of them, to
 * `volatileBackup`, and everything else to `primary`.
 */
export function makePolicy(config: {
  excludedEntityTypes: ReadonlySet<EARS.Entity>;
  hydratePartitions?: ReadonlySet<Partition>;
}): PartitionPolicy {
  const excluded = config.excludedEntityTypes;
  const hydrate = config.hydratePartitions ?? new Set<Partition>(['primary']);

  return {
    routeEntity(entityType) {
      if (excluded.has(entityType)) return 'volatileBackup';
      return 'primary';
    },

    routeRelation({ srcType, tgtType }) {
      if (excluded.has(srcType) || excluded.has(tgtType)) {
        return 'volatileBackup';
      }
      return 'primary';
    },

    hydrate,
  };
}
