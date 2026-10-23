import { EARS } from './entities.ts';
import type { EdgeStore } from './edge-store.ts';
import type { AttributeStorage } from './attribute-storage.ts';
import type { NameChecks } from './names.ts';

/** Which relations to match: any combination of kind, source and target */
export interface RelationMatch {
  kind?: EARS.RelKind;
  source?: EARS.EntityId;
  target?: EARS.EntityId;
}

/** A relation row: its id (`write(id).destroy()` removes that one relation) and its details, including its info */
export interface RelationRow extends EARS.RelationDetail {
  id: EARS.EntityId;
}

export function createRelationReads({ edgeStore, storage, names }: { edgeStore: EdgeStore; storage: AttributeStorage; names: NameChecks }) {
  function findRelations(match: RelationMatch = {}): RelationRow[] {
    if (match.kind !== undefined) names.relationKinds(match.kind, 'findRelations');
    return edgeStore.relIds(match).flatMap((id) => {
      const detail = storage.getAttr(id, EARS.AttrKind.RelationDetails) as EARS.RelationDetail | undefined;
      return detail ? [{ ...detail, id }] : [];
    });
  }

  return { findRelations };
}
