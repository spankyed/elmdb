import type { EARS } from './entities.ts';

interface DirectionLists {
  [entityId: string]: EARS.EntityId[];
}

/** One relation kind's index: the relations of each source, and of each target */
export interface RelationIndexEntry {
  bySource: DirectionLists;
  byTarget: DirectionLists;
}

/** The relation index of an engine, by relation kind */
export type RelationIndex = Record<string, RelationIndexEntry>;

/** An engine's relation index and its writes */
export function createRelationIndex() {
  const index: RelationIndex = {};

  function clear() {
    Object.keys(index).forEach(key => delete index[key]);
  }

  const ensureEntry = (kind: string): RelationIndexEntry => {
    if (!index[kind]) index[kind] = { bySource: {}, byTarget: {} };
    return index[kind];
  };

  function addToIndex(
    kind: string,
    source: EARS.EntityId,
    target: EARS.EntityId,
    relId: EARS.EntityId,
  ): void {
    const e = ensureEntry(kind);
    if (!e.bySource[source]) e.bySource[source] = [];
    if (!e.byTarget[target]) e.byTarget[target] = [];
    if (!e.bySource[source].includes(relId)) e.bySource[source].push(relId);
    if (!e.byTarget[target].includes(relId)) e.byTarget[target].push(relId);
  }

  function removeFromIndex(
    kind: string,
    source: EARS.EntityId,
    target: EARS.EntityId,
    relId: EARS.EntityId,
  ): void {
    const e = index[kind];
    if (!e) return;
    if (e.bySource[source]) e.bySource[source] = e.bySource[source].filter((id) => id !== relId);
    if (e.byTarget[target]) e.byTarget[target] = e.byTarget[target].filter((id) => id !== relId);
    if (e.bySource[source]?.length === 0) delete e.bySource[source];
    if (e.byTarget[target]?.length === 0) delete e.byTarget[target];
    if (Object.keys(e.bySource).length === 0 && Object.keys(e.byTarget).length === 0) {
      delete index[kind];
    }
  }

  function updateIndex(
    kind: string,
    relId: EARS.EntityId,
    oldSource: EARS.EntityId,
    oldTarget: EARS.EntityId,
    newSource?: EARS.EntityId,
    newTarget?: EARS.EntityId,
  ): void {
    const finalSource = newSource ?? oldSource;
    const finalTarget = newTarget ?? oldTarget;
    if (finalSource === oldSource && finalTarget === oldTarget) return;
    removeFromIndex(kind, oldSource, oldTarget, relId);
    addToIndex(kind, finalSource, finalTarget, relId);
  }

  return { index, clear, addToIndex, removeFromIndex, updateIndex };
}

export type RelationIndexStore = ReturnType<typeof createRelationIndex>;
