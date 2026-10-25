// The read sets of every open watch, indexed so a commit finds the watches it touched without visiting the rest: the
// keys reads got in a map, and the ranges they scanned in an interval treap. A written key costs one map lookup and a
// stabbing query of O(log N + matches), N being the ranges held, so a commit's cost doesn't grow with the views open.
import type { ReadSet } from './kv.ts';

/** Above every byte string (each character is a byte, below `Ā`): the end of a range that runs to the store's end */
const TOP = 'Ā';

/**
 * A range in the treap: ordered by `low`, ties by `id`; `max` is the highest `high` in its subtree, which is what lets a
 * stabbing query skip a subtree no range of which reaches the key
 */
interface Node<W> {
  low: string;
  high: string;
  id: number;
  owner: W;
  priority: number;
  max: string;
  left: Node<W> | undefined;
  right: Node<W> | undefined;
}

/** A treap's split and merge, over nodes ordered by `(low, id)` */
const below = <W>(node: Node<W>, low: string, id: number) => node.low < low || (node.low === low && node.id < id);

function refresh<W>(node: Node<W>): Node<W> {
  let max = node.high;
  if (node.left && node.left.max > max) max = node.left.max;
  if (node.right && node.right.max > max) max = node.right.max;
  node.max = max;
  return node;
}

/** The nodes below `(low, id)`, and the rest */
function split<W>(node: Node<W> | undefined, low: string, id: number): [Node<W> | undefined, Node<W> | undefined] {
  if (!node) return [undefined, undefined];
  if (below(node, low, id)) {
    const [left, right] = split(node.right, low, id);
    node.right = left;
    return [refresh(node), right];
  }
  const [left, right] = split(node.left, low, id);
  node.left = right;
  return [left, refresh(node)];
}

/** Two treaps as one, every node of `a` ordered before every node of `b` */
function merge<W>(a: Node<W> | undefined, b: Node<W> | undefined): Node<W> | undefined {
  if (!a) return b;
  if (!b) return a;
  if (a.priority > b.priority) {
    a.right = merge(a.right, b);
    return refresh(a);
  }
  b.left = merge(a, b.left);
  return refresh(b);
}

/** Calls `found` with the owner of every range in `node` holding `key`: `low <= key < high` */
function stab<W>(node: Node<W> | undefined, key: string, found: (owner: W) => void): void {
  if (!node || node.max <= key) return;
  stab(node.left, key, found);
  if (node.low > key) return;
  if (key < node.high) found(node.owner);
  stab(node.right, key, found);
}

/** What an owner holds in the index, so it can take it out again */
interface Entry<W> { keys: readonly string[]; nodes: Array<Node<W>> }

export interface ReadSetIndex<W> {
  /** Files `reads` under `owner`, replacing what the owner had filed */
  set(owner: W, reads: ReadSet): void;
  /** Takes the owner's read set out */
  remove(owner: W): void;
  /** Every owner whose read set holds a key in `keys` (each once), or every owner for `all` */
  touchedBy(keys: ReadonlySet<string> | 'all'): Set<W>;
  /** How many keys and ranges are filed: what a spec checks is left once every owner is removed */
  size(): { keys: number; ranges: number };
}

export function createReadSetIndex<W>(): ReadSetIndex<W> {
  const keys = new Map<string, Set<W>>();
  let root: Node<W> | undefined;
  let ranges = 0;
  let nextId = 0;
  const entries = new Map<W, Entry<W>>();

  const remove = (owner: W) => {
    const entry = entries.get(owner);
    if (!entry) return;
    entries.delete(owner);
    for (const key of entry.keys) {
      const owners = keys.get(key)!;
      owners.delete(owner);
      if (owners.size === 0) keys.delete(key);
    }
    for (const node of entry.nodes) {
      const [before, rest] = split(root, node.low, node.id);
      const [, after] = split(rest, node.low, node.id + 1);
      root = merge(before, after);
      ranges--;
    }
  };

  return {
    set(owner, reads) {
      remove(owner);
      const entry: Entry<W> = { keys: [...reads.keys], nodes: [] };
      for (const key of entry.keys) {
        let owners = keys.get(key);
        if (!owners) keys.set(key, owners = new Set());
        owners.add(owner);
      }
      for (const [low, high = TOP] of reads.ranges) {
        if (low >= high) continue;
        const node: Node<W> = { low, high, id: nextId++, owner, priority: Math.random(), max: high, left: undefined, right: undefined };
        const [before, after] = split(root, low, node.id);
        root = merge(merge(before, node), after);
        entry.nodes.push(node);
        ranges++;
      }
      entries.set(owner, entry);
    },
    remove,
    touchedBy(written) {
      if (written === 'all') return new Set(entries.keys());
      const touched = new Set<W>();
      const add = (owner: W) => { touched.add(owner); };
      for (const key of written) {
        const owners = keys.get(key);
        if (owners) for (const owner of owners) touched.add(owner);
        stab(root, key, add);
      }
      return touched;
    },
    size: () => ({ keys: keys.size, ranges }),
  };
}
