import type { Query } from './query.ts';
import { EARS } from '../entities.ts';
import type { NameChecks } from '../names.ts';

/** An engine's cycle check over a hierarchy — a tree or any graph of its relation kinds — over its queries */
export function createHierarchy({ query, names }: { query: Query; names: NameChecks }) {
  const children = (id: EARS.EntityId, kinds: readonly EARS.RelKind[]): EARS.EntityId[] =>
    kinds.flatMap(k => query.ids(id).out(k).ids());

  /** Whether linking `source` to `target` would close a cycle through `kinds`: `target` already reaches `source`, or is `source` */
  function wouldCreateCycle(
    source: EARS.EntityId,
    target: EARS.EntityId,
    kinds: readonly EARS.RelKind[],
  ): boolean {
    names.relationKinds(kinds, 'wouldCreateCycle');
    const seen = new Set<EARS.EntityId>();
    const stack = [target];
    while (stack.length) {
      const node = stack.pop()!;
      if (node === source) return true;
      for (const n of children(node, kinds)) {
        if (!seen.has(n)) {
          seen.add(n);
          stack.push(n);
        }
      }
    }
    return false;
  }

  return { wouldCreateCycle };
}
