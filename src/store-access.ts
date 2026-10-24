import type { KvReader, KvWriter, OrderedKV } from './kv.ts';

/**
 * How an engine's modules reach its store: `read` runs in the read snapshot or write unit already open, or opens a
 * snapshot for itself, and `write` runs in a unit, nested in the one open. So a query's terminal opens one snapshot
 * and every read its steps make answers from it, a unit's reads see its writes, and a write a `where` predicate makes
 * during a walk commits in a unit of its own, which the walk's snapshot doesn't see.
 *
 * An engine operation's writes join the unit already open (`write`), as one statement joins its transaction; only a
 * caller's unit (`unit`, what `transact` runs in) is a savepoint inside another, undoing its own writes when it throws.
 */
export interface StoreAccess {
  read<T>(fn: (reader: KvReader) => T): T;
  /** Runs `fn` in the write unit already open, or in a unit of its own */
  write<T>(fn: (writer: KvWriter) => T): T;
  /** Runs `fn` as a unit: in one of its own, or as a savepoint inside the one open, which a throw undoes alone */
  unit<T>(fn: (writer: KvWriter) => T): T;
  /** The next number of the store's one sequence: what orders entities and links as they were made */
  nextSeq(writer: KvWriter): number;
  /** The last number the sequence handed out, or 0 */
  lastSeq(reader: KvReader): number;
}

const SEQ = ['m', 'seq'] as const;

export function createStoreAccess(kv: OrderedKV): StoreAccess {
  /** The open snapshots and units, innermost last: what a read without its own reaches */
  const open: KvReader[] = [];
  /** The open unit's writer, the innermost savepoint's while one is open */
  const writers: KvWriter[] = [];
  const within = <R extends KvReader, T>(reader: R, fn: (reader: R) => T): T => {
    open.push(reader);
    try {
      return fn(reader);
    } finally {
      open.pop();
    }
  };
  const unit: StoreAccess['unit'] = (fn) => kv.write((writer) => {
    writers.push(writer);
    try {
      return within(writer, fn);
    } finally {
      writers.pop();
    }
  });
  return {
    read(fn) {
      const current = open[open.length - 1];
      return current ? fn(current) : kv.read((reader) => within(reader, fn));
    },
    write(fn) {
      const writer = writers[writers.length - 1];
      return writer ? within(writer, fn) : unit(fn);
    },
    unit,
    nextSeq(writer) {
      const next = ((writer.get(SEQ) as number | undefined) ?? 0) + 1;
      writer.put(SEQ, next);
      return next;
    },
    lastSeq: (reader) => (reader.get(SEQ) as number | undefined) ?? 0,
  };
}
