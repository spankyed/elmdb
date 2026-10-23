import type { Query } from './query.ts';
import { EARS } from './entities.ts';
import type { NameChecks } from './names.ts';

/** An engine's walks over a hierarchy — a tree or any graph of one relation kind — over its queries */
export function createGraph({ query, names }: { query: Query; names: NameChecks }) {
  const children = (id: EARS.EntityId, kinds: readonly EARS.RelKind[]): EARS.EntityId[] =>
    kinds.flatMap(k => query.ids(id).out(k).ids());

  /** Every entity `start` reaches through `relKind`, each once, `start` excluded: a subtree, or a cycle's other members */
  function descendants(
    start: EARS.EntityId,
    relKind: EARS.RelKind,
  ): EARS.EntityId[] {
    names.relationKinds(relKind, 'descendants');
    const seen = new Set<EARS.EntityId>();
    const stack = [start];

    while (stack.length) {
      const node = stack.pop()!;
      for (const child of query.ids(node).out(relKind).ids()) {
        if (!seen.has(child)) {
          seen.add(child);
          stack.push(child);
        }
      }
    }
    seen.delete(start);
    return [...seen];
  }

  /** Every entity that reaches `start` through `relKind`, each once, `start` excluded: in a tree, its parent first and its root last */
  function ancestors(
    start: EARS.EntityId,
    relKind: EARS.RelKind,
  ): EARS.EntityId[] {
    names.relationKinds(relKind, 'ancestors');
    const seen = new Set<EARS.EntityId>();
    const stack = [start];

    while (stack.length) {
      const node = stack.pop()!;
      for (const parent of query.ids(node)
        .in(relKind)
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

  /** Whether linking `src` to `tgt` would close a cycle through `kinds`: `tgt` already reaches `src`, or is `src` */
  function wouldCreateCycle(
    src: EARS.EntityId,
    tgt: EARS.EntityId,
    kinds: readonly EARS.RelKind[],
  ): boolean {
    names.relationKinds(kinds, 'wouldCreateCycle');
    const seen = new Set<EARS.EntityId>();
    const stack = [tgt];
    while (stack.length) {
      const node = stack.pop()!;
      if (node === src) return true;
      for (const n of children(node, kinds)) {
        if (!seen.has(n)) {
          seen.add(n);
          stack.push(n);
        }
      }
    }
    return false;
  }

  return { descendants, ancestors, wouldCreateCycle };
}
