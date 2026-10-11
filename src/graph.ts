import type { qx as Qx } from './query.ts';
import type { EdgeStore } from './edge-store.ts';
import { EARS } from './entities.ts';
import { installedEngine } from './installed.ts';

/** An engine's graph walks, over its queries and edge store */
export function createGraph({ qx, edgeStore }: { qx: typeof Qx; edgeStore: EdgeStore }) {
  const neighbours = (
    id: EARS.EntityId,
    kinds: readonly EARS.RelKind[],
    asSource = true,
  ): EARS.EntityId[] =>
    kinds.flatMap(k =>
      qx(id).linksTo(k, undefined, asSource).ids(),
    );

  function descendants(
    start: EARS.EntityId,
    relKind: EARS.RelKind,
  ): EARS.EntityId[] {
    const seen = new Set<EARS.EntityId>();
    const stack = [start];

    while (stack.length) {
      const node = stack.pop()!;
      for (const child of qx(node).linksTo(relKind).ids()) {
        if (!seen.has(child)) {
          seen.add(child);
          stack.push(child);
        }
      }
    }
    seen.delete(start);
    return [...seen];
  }

  function ancestors(
    start: EARS.EntityId,
    relKind: EARS.RelKind,
  ): EARS.EntityId[] {
    const seen = new Set<EARS.EntityId>();
    const stack = [start];

    while (stack.length) {
      const node = stack.pop()!;
      for (const parent of qx(node)
        .linksTo(relKind, undefined, false)
        .ids()) {
        if (!seen.has(parent)) {
          seen.add(parent);
          stack.push(parent);
        }
      }
    }
    seen.delete(start);
    return [...seen];
  }

  function rootParent(
    start: EARS.EntityId,
    relKind: EARS.RelKind,
  ): EARS.EntityId {
    let cur = start;
    while (true) {
      const parent = qx(cur)
        .linksTo(relKind, undefined, false)
        .first();
      if (!parent) break;
      cur = parent;
    }
    return cur;
  }

  function wouldCreateCycle(
    src: EARS.EntityId,
    tgt: EARS.EntityId,
    kinds: readonly EARS.RelKind[],
  ): boolean {
    const seen = new Set<EARS.EntityId>();
    const stack = [tgt];
    while (stack.length) {
      const node = stack.pop()!;
      if (node === src) return true;
      for (const n of neighbours(node, kinds)) {
        if (!seen.has(n)) {
          seen.add(n);
          stack.push(n);
        }
      }
    }
    return false;
  }

  function linkSymmetric(
    a: EARS.EntityId,
    b: EARS.EntityId,
    kind: EARS.RelKind,
    info?: unknown,
  ) {
    if (a === b) throw new Error("Self-link refused");
    edgeStore.linkOne(a, kind, b, info);
    edgeStore.linkOne(b, kind, a, info);
  }

  function topoSort(
    roots: EARS.EntityId[],
    kind: EARS.RelKind,
    filterType?: EARS.Entity,
  ): EARS.EntityId[] {
    const outgoing = new Map<EARS.EntityId, Set<EARS.EntityId>>();
    const incoming = new Map<EARS.EntityId, number>();

    const all = new Set<EARS.EntityId>(roots);
    const collect = (id: EARS.EntityId) => {
      const kids = new Set<EARS.EntityId>(qx(id).linksTo(kind, filterType).ids());
      outgoing.set(id, kids);
      kids.forEach(k => {
        incoming.set(k, (incoming.get(k) ?? 0) + 1);
        if (!outgoing.has(k)) collect(k);
        all.add(k);
      });
    };
    roots.forEach(collect);

    const queue = [...all].filter(id => !incoming.has(id));
    const ordered: EARS.EntityId[] = [];

    while (queue.length) {
      const n = queue.shift()!;
      ordered.push(n);
      for (const m of outgoing.get(n) ?? []) {
        incoming.set(m, (incoming.get(m) ?? 0) - 1);
        if (incoming.get(m) === 0) queue.push(m);
      }
    }
    if (ordered.length !== all.size)
      throw new Error("Cycle detected – topological order impossible");
    return ordered;
  }

  function shortestPath(src: EARS.EntityId, tgt: EARS.EntityId, kinds: EARS.RelKind[]): EARS.EntityId[] | null {
    if (src === tgt) return [src];

    const front = new Map<EARS.EntityId, EARS.EntityId>(); front.set(src, src);
    const back = new Map<EARS.EntityId, EARS.EntityId>(); back.set(tgt, tgt);

    const fwdNbrs = (id: EARS.EntityId) => neighbours(id, kinds, true);
    const bckNbrs = (id: EARS.EntityId) => neighbours(id, kinds, false);

    let layer = [src], revLayer = [tgt];

    while (layer.length && revLayer.length) {
      if (layer.length <= revLayer.length) {
        const next: EARS.EntityId[] = [];
        for (const v of layer)
          for (const n of fwdNbrs(v))
            if (!front.has(n)) {
              front.set(n, v);
              if (back.has(n)) return join(n);
              next.push(n);
            }
        layer = next;
      } else {
        const next: EARS.EntityId[] = [];
        for (const v of revLayer)
          for (const n of bckNbrs(v))
            if (!back.has(n)) {
              back.set(n, v);
              if (front.has(n)) return join(n);
              next.push(n);
            }
        revLayer = next;
      }
    }
    return null;

    function join(meet: EARS.EntityId): EARS.EntityId[] {
      const path = [meet];
      for (let v = meet; v !== src; v = front.get(v)!) path.unshift(front.get(v)!);
      for (let v = meet; v !== tgt; v = back.get(v)!)  path.push(back.get(v)!);
      return path;
    }
  }

  function leaves(
    kind: EARS.RelKind,
    filterType?: EARS.Entity,
  ): EARS.EntityId[] {
    const all = new Set<EARS.EntityId>(qx(filterType).ids());
    const hasOut = new Set<EARS.EntityId>();

    for (const id of all) {
      if (qx(id).linksTo(kind, filterType).exists()) {
        hasOut.add(id);
      }
    }
    return [...all].filter(id => !hasOut.has(id));
  }

  function lowestCommonAncestor(
    a: EARS.EntityId,
    b: EARS.EntityId,
    treeKind: EARS.RelKind,
  ): EARS.EntityId | null {
    const pathToRoot = (id: EARS.EntityId) => {
      const path: EARS.EntityId[] = [];
      let cur: EARS.EntityId | null = id;
      while (cur) {
        path.push(cur);
        cur = qx(cur)
          .linksTo(treeKind, undefined, false)
          .first();
      }
      return path;
    };

    const A = pathToRoot(a);
    const B = new Set(pathToRoot(b));
    return A.find(node => B.has(node)) ?? null;
  }

  return {
    descendants, ancestors, rootParent, wouldCreateCycle, linkSymmetric,
    topoSort, shortestPath, leaves, lowestCommonAncestor,
  };
}

// The installed engine's graph walks (see installed.ts)

export function descendants(
  start: EARS.EntityId,
  relKind: EARS.RelKind,
): EARS.EntityId[] {
  return installedEngine().descendants(start, relKind);
}

export function ancestors(
  start: EARS.EntityId,
  relKind: EARS.RelKind,
): EARS.EntityId[] {
  return installedEngine().ancestors(start, relKind);
}

export function rootParent(
  start: EARS.EntityId,
  relKind: EARS.RelKind,
): EARS.EntityId {
  return installedEngine().rootParent(start, relKind);
}

export function wouldCreateCycle(
  src: EARS.EntityId,
  tgt: EARS.EntityId,
  kinds: readonly EARS.RelKind[],
): boolean {
  return installedEngine().wouldCreateCycle(src, tgt, kinds);
}

export function linkSymmetric(
  a: EARS.EntityId,
  b: EARS.EntityId,
  kind: EARS.RelKind,
  info?: unknown,
) {
  installedEngine().linkSymmetric(a, b, kind, info);
}

export function topoSort(
  roots: EARS.EntityId[],
  kind: EARS.RelKind,
  filterType?: EARS.Entity,
): EARS.EntityId[] {
  return installedEngine().topoSort(roots, kind, filterType);
}

export function shortestPath(src: EARS.EntityId, tgt: EARS.EntityId, kinds: EARS.RelKind[]): EARS.EntityId[] | null {
  return installedEngine().shortestPath(src, tgt, kinds);
}

export function leaves(
  kind: EARS.RelKind,
  filterType?: EARS.Entity,
): EARS.EntityId[] {
  return installedEngine().leaves(kind, filterType);
}

export function lowestCommonAncestor(
  a: EARS.EntityId,
  b: EARS.EntityId,
  treeKind: EARS.RelKind,
): EARS.EntityId | null {
  return installedEngine().lowestCommonAncestor(a, b, treeKind);
}
