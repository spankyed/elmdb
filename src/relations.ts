import { EARS } from './entities.ts';
import type { EdgeStore } from './edge-store.ts';
import type { AttributeStorage } from './attribute-storage.ts';
import type { NameChecks } from './names.ts';
import type { RelationEdge } from './runtime.ts';

/** Which relations to match: any combination of kind, source and target */
export interface RelationMatch {
  kind?: EARS.RelKind;
  source?: EARS.EntityId;
  target?: EARS.EntityId;
}

/**
 * A relation row: its id (`write(id).destroy()` removes that one relation), kind, ends and info. The row
 * `RelationEdge` types, with its kind and ends untyped
 */
export type RelationRow = RelationEdge;

export function createRelationReads({ edgeStore, storage, names }: { edgeStore: EdgeStore; storage: AttributeStorage; names: NameChecks }) {
  function findRelations(match: RelationMatch = {}): RelationRow[] {
    if (match.kind !== undefined) names.relationKinds(match.kind, 'findRelations');
    return edgeStore.relIds(match).flatMap((id) => {
      const detail = storage.relationOf(id);
      return detail ? [{ ...detail, id }] : [];
    });
  }

  return { findRelations };
}
