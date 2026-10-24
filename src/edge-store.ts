import type { RelationIndexStore } from './relation-index.ts';
import type { AttributeStorage } from './attribute-storage.ts';
import { EARS } from './entities.ts';

/** Which relations an edge store call matches: any combination of source, kind and target */
export type EdgeMatch = Partial<Pick<EARS.RelationDetail, 'source' | 'kind' | 'target'>>;

/** Relation reads and writes by their ends */
export interface EdgeStore {
  /** The ids of the matching relations */
  relIds(w: EdgeMatch): EARS.EntityId[];
  /** The matching relations' details */
  find(w: Partial<EARS.RelationDetail>): EARS.RelationDetail[];
  /** Removes the matching relations */
  unlink(w: Partial<EARS.RelationDetail>): void;
  /** Replaces the relations between `source` and `target` of `kind` with one; returns its id */
  linkOne(source: EARS.EntityId, kind: EARS.RelKind, target: EARS.EntityId, info?: unknown): EARS.EntityId;
  /** Moves the first matching relation; whether one matched */
  patchOne(w: Partial<EARS.RelationDetail>, u: { newSource?: EARS.EntityId; newTarget?: EARS.EntityId; newInfo?: unknown }): boolean;
}

export function createEdgeStore({ relations, storage }: { relations: RelationIndexStore; storage: AttributeStorage }): EdgeStore {
  const relationIndex = relations.index;

  const matchIds = (w: EdgeMatch): EARS.EntityId[] => {
    const kinds = w.kind ? [w.kind] : Object.keys(relationIndex);
    const out   = new Set<EARS.EntityId>();

    for (const k of kinds) {
      const { bySource, byTarget } = relationIndex[k] ?? {};
      if (!bySource) continue;

      if (w.source !== undefined && w.target !== undefined) {
        const fromSource = new Set(bySource[w.source] ?? []);
        const fromTarget = new Set(byTarget[w.target] ?? []);
        fromSource.forEach(id => {
          if (fromTarget.has(id)) out.add(id);
        });
      }
      else if (w.source !== undefined) {
        (bySource[w.source] ?? []).forEach(id => out.add(id));
      }
      else if (w.target !== undefined) {
        (byTarget[w.target] ?? []).forEach(id => out.add(id));
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

    linkOne: (source, kind, target, info) => {
      edgeStore.unlink({
        source,
        kind,
        target,
      });
      return storage.addRelation(source, kind, target, info);
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
