import type { RelationIndexStore } from './relation-index.ts';
import type { AttributeStorage } from './attribute-storage.ts';
import { EARS } from './entities.ts';

/** Which relations an edge store call matches: any combination of source, kind and target */
export type EdgeMatch = Partial<Pick<EARS.RelationDetail, 'sourceEntity' | 'relationType' | 'targetEntity'>>;

/** Relation reads and writes by their ends */
export interface EdgeStore {
  /** The ids of the matching relations */
  relIds(w: EdgeMatch): EARS.EntityId[];
  /** The matching relations' details */
  find(w: Partial<EARS.RelationDetail>): EARS.RelationDetail[];
  /** Removes the matching relations */
  unlink(w: Partial<EARS.RelationDetail>): void;
  /** Replaces the relations between `src` and `tgt` of `kind` with one; returns its id */
  linkOne(src: EARS.EntityId, kind: EARS.RelKind, tgt: EARS.EntityId, info?: unknown): EARS.EntityId;
  /** Moves the first matching relation; whether one matched */
  patchOne(w: Partial<EARS.RelationDetail>, u: { newSource?: EARS.EntityId; newTarget?: EARS.EntityId; newInfo?: unknown }): boolean;
}

export function createEdgeStore({ relations, storage }: { relations: RelationIndexStore; storage: AttributeStorage }): EdgeStore {
  const relationIndex = relations.index;

  const matchIds = (w: EdgeMatch): EARS.EntityId[] => {
    const kinds = w.relationType ? [w.relationType] : Object.keys(relationIndex);
    const out   = new Set<EARS.EntityId>();

    for (const k of kinds) {
      const { bySource, byTarget } = relationIndex[k] ?? {};
      if (!bySource) continue;

      if (w.sourceEntity !== undefined && w.targetEntity !== undefined) {
        const fromSource = new Set(bySource[w.sourceEntity] ?? []);
        const fromTarget = new Set(byTarget[w.targetEntity] ?? []);
        fromSource.forEach(id => {
          if (fromTarget.has(id)) out.add(id);
        });
      }
      else if (w.sourceEntity !== undefined) {
        (bySource[w.sourceEntity] ?? []).forEach(id => out.add(id));
      }
      else if (w.targetEntity !== undefined) {
        (byTarget[w.targetEntity] ?? []).forEach(id => out.add(id));
      }
      else {
        for (const ids of Object.values(bySource)) {
          for (const id of ids) {
            out.add(id);
          }
        }
      }
    }
    return [...out];
  };

  const edgeStore: EdgeStore = {
    relIds: (w) =>
      matchIds(w),

    find: (w) =>
      matchIds(w).map(
        id => storage.getAttr(id, EARS.AttrKind.RelationDetails) as EARS.RelationDetail,
      ),

    unlink: (w) =>
      matchIds(w).forEach(storage.removeRelationById),

    linkOne: (src, kind, tgt, info) => {
      edgeStore.unlink({
        sourceEntity: src,
        relationType: kind,
        targetEntity: tgt,
      });
      return storage.addRelation(src, kind, tgt, info);
    },

    patchOne: (w, u) => {
      const [relId] = matchIds(w);
      if (relId) {
        storage.updateRelation(relId, u.newSource, u.newTarget, u.newInfo);
        return true;
      }
      return false;
    },
  };
  return edgeStore;
}
