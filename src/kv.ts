/**
 * The ordered key-value seam the engine is written on: keys are arrays, ordered as LMDB orders them, and values are
 * any structured value, decoded fresh on every read. Two backends sit under it — LMDB (`@apackdev/ears/lmdb`) for the
 * app and `createMemoryKV` for tests and tooling — and both store bytes this module encodes: keys with
 * the codec below and values with `msgpackr`, the value encoding LMDB itself uses. So a key sorts, and a value round-trips,
 * the same on either, and a test on memory is a test of what the app stores.
 *
 * Everything that is the same on both lives here, over the bytes: prefix ranges, nested units (savepoints), and a unit
 * reading its own writes. A backend supplies a read snapshot, a write unit that commits or aborts whole, and the
 * bytes.
 */
import { Packr } from 'msgpackr';

/** One element of a key, ordered null, then booleans, then numbers, then strings */
export type KeyPart = string | number | boolean | null;
/** A key: its elements, compared element by element */
export type Key = readonly KeyPart[];

/** Which keys a range visits */
export interface RangeOptions {
  /** Every key with these leading elements; with `start`/`end`, the range is narrowed within it */
  prefix?: Key;
  /** The lowest key visited (inclusive), whatever the direction */
  start?: Key;
  /** The key the range stops before (exclusive), whatever the direction */
  end?: Key;
  /** Highest key first */
  reverse?: boolean;
  /** How many entries to visit at most */
  limit?: number;
  /** False to visit keys only, without decoding a value */
  values?: boolean;
}

/** What a visit is given, and answers: whether to go on */
export type Visit = (key: Key, value: unknown) => boolean | void;

/** Reads, inside one read snapshot or one write unit */
export interface KvReader {
  /** The value at `key`, or undefined when there is none */
  get(key: Key): unknown;
  /** Visits the range's entries in key order (highest first with `reverse`) until `visit` answers false */
  range(options: RangeOptions, visit: Visit): void;
}

/** Reads and writes, inside one write unit: its reads see its writes */
export interface KvWriter extends KvReader {
  put(key: Key, value: unknown): void;
  /** Removes the entry at `key`; a key with none is ignored */
  del(key: Key): void;
}

/** An ordered key-value store: read snapshots and write units over it */
export interface OrderedKV {
  /**
   * Runs `fn` over one read snapshot: every read it makes answers from the same moment, whatever commits meanwhile.
   * Inside a write unit it reads the unit, its own writes included.
   */
  read<T>(fn: (reader: KvReader) => T): T;
  /**
   * Runs `fn` as one write unit: committed whole when it returns, nothing kept when it throws. A unit inside another
   * commits into it and, throwing, undoes only its own writes.
   */
  write<T>(fn: (writer: KvWriter) => T): T;
  /** Whether a write unit is open */
  readonly writing: boolean;
}

/** A backend's bytes, read: a key's value, and a range of keys in byte order */
export interface ByteReader {
  get(key: Uint8Array): Uint8Array | undefined;
  /**
   * Visits the keys from `low` (inclusive) to `high` (exclusive), highest first with `reverse`, until `visit` answers
   * false. A range is closed however the visit ends — returning, answering false or throwing
   */
  scan(low: Uint8Array, high: Uint8Array | undefined, reverse: boolean, values: boolean, visit: (key: Uint8Array, value: Uint8Array | undefined) => boolean): void;
}

/** A backend's bytes, written inside one unit */
export interface ByteWriter extends ByteReader {
  put(key: Uint8Array, value: Uint8Array): void;
  del(key: Uint8Array): void;
}

/** What a backend supplies: a read snapshot, and a write unit that commits on return and aborts on a throw */
export interface KvBackend {
  read<T>(fn: (reader: ByteReader) => T): T;
  write<T>(fn: (writer: ByteWriter) => T): T;
}

// ─── Encoding ─────────────────────────────────────────────────────────────

/**
 * The key codec. Each element is a tag byte and its bytes, so byte order is the order of the elements, element by
 * element: `null`, `false`, `true`, a number (eight bytes of its float64, sign flipped, negatives inverted, so their
 * order is the numbers'), a string (its UTF-8 and a 0 that ends it, so `['a']` sorts before `['a', …]` and before
 * `['ab']`, and is no byte prefix of the latter). A string holding a NUL is refused, having no end to mark.
 *
 * A key's elements follow its prefix's bytes with a tag, so the keys under a prefix lie between the prefix and the
 * lowest tag (`TAG_LOW`) and the prefix and one past the highest (`TAG_HIGH`): the prefix's own key is outside them.
 */
const TAG = { null: 1, false: 2, true: 3, number: 4, string: 5 } as const;
const TAG_LOW = 1;
const TAG_HIGH = 6;

/** An encoder for keys, reusing one buffer and copying out what a key took, optionally followed by a byte */
function keyEncoder() {
  let buffer = Buffer.allocUnsafe(1024);
  const view = () => new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);
  let data = view();
  const room = (needed: number, at: number) => {
    if (at + needed <= buffer.length) return;
    const grown = Buffer.allocUnsafe(Math.max(buffer.length * 2, at + needed));
    buffer.copy(grown, 0, 0, at);
    buffer = grown;
    data = view();
  };
  return (key: Key, then?: number): Uint8Array => {
    let at = 0;
    for (const part of key) {
      if (part === null) { room(1, at); buffer[at++] = TAG.null; }
      else if (part === false) { room(1, at); buffer[at++] = TAG.false; }
      else if (part === true) { room(1, at); buffer[at++] = TAG.true; }
      else if (typeof part === 'number') {
        if (Number.isNaN(part)) throw new Error('A key holds no NaN: it has no place in an order');
        room(9, at);
        buffer[at++] = TAG.number;
        data.setFloat64(at, part === 0 ? 0 : part);
        if (part < 0) for (let i = 0; i < 8; i++) buffer[at + i] = ~buffer[at + i]! & 0xff;
        else buffer[at] = buffer[at]! | 0x80;
        at += 8;
      } else {
        if (part.includes('\u0000')) throw new Error(`A key's string holds no NUL: ${JSON.stringify(part)}`);
        const length = Buffer.byteLength(part, 'utf8');
        room(length + 2, at);
        buffer[at++] = TAG.string;
        at += buffer.write(part, at, 'utf8');
        buffer[at++] = 0;
      }
    }
    if (then !== undefined) { room(1, at); buffer[at++] = then; }
    // A copy: a Buffer's own slice is a view, which the next key would overwrite
    return Buffer.from(buffer.subarray(0, at));
  };
}

/** A key from its bytes */
const decodeKey = (bytes: Uint8Array): Key => {
  const buffer = Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const parts: KeyPart[] = [];
  let at = 0;
  while (at < buffer.length) {
    const tag = buffer[at++];
    if (tag === TAG.string) {
      const end = buffer.indexOf(0, at);
      parts.push(buffer.toString('utf8', at, end));
      at = end + 1;
    } else if (tag === TAG.number) {
      const bits = Buffer.from(buffer.subarray(at, at + 8));
      if (bits[0]! & 0x80) bits[0] = bits[0]! & 0x7f;
      else for (let i = 0; i < 8; i++) bits[i] = ~bits[i]! & 0xff;
      parts.push(bits.readDoubleBE(0));
      at += 8;
    } else {
      parts.push(tag === TAG.null ? null : tag === TAG.true);
    }
  }
  return parts;
};

/**
 * A decoded value with each msgpack map an object again. Maps are decoded as `Map`s, since decoding straight to an object
 * renames a `__proto__` key (`msgpackr` would set the prototype otherwise); here it is defined as an own property, so a
 * value comes back as it was stored
 */
function objectsOf(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(objectsOf);
  if (!(value instanceof Map)) return value;
  const object: Record<string, unknown> = {};
  for (const [key, field] of value) {
    const name = String(key);
    if (name === '__proto__') Object.defineProperty(object, name, { value: objectsOf(field), writable: true, enumerable: true, configurable: true });
    else object[name] = objectsOf(field);
  }
  return object;
}


// ─── The seam over a backend ──────────────────────────────────────────────

/** What a nested unit undoes when it throws: each key it first wrote, and what that key held before (undefined: none) */
type Savepoint = Map<string, Uint8Array | undefined>;

/** Bytes as a string of the same length, one character a byte, so string order is byte order */
const byteString = (bytes: Uint8Array) => Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString('latin1');

/**
 * The seam over a backend: keys and values encoded, prefix ranges resolved, a unit's reads going through it, and a
 * nested unit's writes undone when it throws.
 */
export function createOrderedKV(backend: KvBackend): OrderedKV {
  const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: false });
  const encodeKey = keyEncoder();
  const encodeValue = (value: unknown): Uint8Array => packr.pack(value);
  const decodeValue = (bytes: Uint8Array): unknown => objectsOf(packr.unpack(bytes));

  /** The open unit's writer and its savepoints, innermost last; none while no unit is open */
  let unit: { writer: ByteWriter; savepoints: Savepoint[] } | undefined;

  const bounds = ({ prefix, start, end }: RangeOptions): [Uint8Array, Uint8Array | undefined] => {
    if (prefix && prefix.length > 0) {
      return [start ? encodeKey([...prefix, ...start]) : encodeKey(prefix, TAG_LOW), end ? encodeKey([...prefix, ...end]) : encodeKey(prefix, TAG_HIGH)];
    }
    return [start ? encodeKey(start) : new Uint8Array(0), end ? encodeKey(end) : undefined];
  };

  const reader = (bytes: ByteReader): KvReader => ({
    get(key) {
      const value = bytes.get(encodeKey(key));
      return value === undefined ? undefined : decodeValue(value);
    },
    range(options, visit) {
      const [low, high] = bounds(options);
      const values = options.values !== false;
      let left = options.limit ?? Infinity;
      if (left <= 0) return;
      bytes.scan(low, high, options.reverse === true, values, (key, value) => {
        if (visit(decodeKey(key), values && value !== undefined ? decodeValue(value) : undefined) === false) return false;
        return --left > 0;
      });
    },
  });

  const writer = (bytes: ByteWriter, savepoints: Savepoint[]): KvWriter => {
    /** Records, in the innermost savepoint, what `key` held before its first write there */
    const remember = (key: Uint8Array) => {
      const savepoint = savepoints[savepoints.length - 1];
      if (!savepoint) return;
      const name = byteString(key);
      if (!savepoint.has(name)) savepoint.set(name, bytes.get(key));
    };
    return {
      ...reader(bytes),
      put(key, value) {
        if (value === undefined) throw new Error(`put(${JSON.stringify(key)}): no value; del(key) removes one`);
        const encoded = encodeKey(key);
        remember(encoded);
        bytes.put(encoded, encodeValue(value));
      },
      del(key) {
        const encoded = encodeKey(key);
        remember(encoded);
        bytes.del(encoded);
      },
    };
  };

  return {
    get writing() { return unit !== undefined; },
    read(fn) {
      if (unit) return fn(reader(unit.writer));
      return backend.read((bytes) => fn(reader(bytes)));
    },
    write(fn) {
      const open = unit;
      if (open) {
        // A unit inside another: its writes go to the outer unit, and a throw puts back what it changed
        const savepoint: Savepoint = new Map();
        open.savepoints.push(savepoint);
        try {
          const result = fn(writer(open.writer, open.savepoints));
          open.savepoints.pop();
          // The outer unit may still throw: what this one changed is the outer's to undo, from what it found first
          const outer = open.savepoints[open.savepoints.length - 1];
          if (outer) for (const [key, before] of savepoint) if (!outer.has(key)) outer.set(key, before);
          return result;
        } catch (error) {
          open.savepoints.pop();
          for (const [name, before] of savepoint) {
            const key = Uint8Array.from(Buffer.from(name, 'latin1'));
            if (before === undefined) open.writer.del(key);
            else open.writer.put(key, before);
          }
          throw error;
        }
      }
      return backend.write((bytes) => {
        unit = { writer: bytes, savepoints: [] };
        try {
          return fn(writer(bytes, unit.savepoints));
        } finally {
          unit = undefined;
        }
      });
    },
  };
}

// ─── Memory ───────────────────────────────────────────────────────────────

/** One committed value of a key: the version it was written at, and the bytes (undefined: removed) */
interface Version { at: number; bytes: Uint8Array | undefined }

/** The first index of `list` whose string is not below `key` */
function lowerBound(list: readonly string[], key: string): number {
  let low = 0;
  let high = list.length;
  while (low < high) {
    const mid = (low + high) >>> 1;
    if (list[mid]! < key) low = mid + 1;
    else high = mid;
  }
  return low;
}

/** How many strings a chunk holds before it splits: an insert moves at most this many */
const CHUNK = 1024;

/**
 * Strings kept sorted in chunks, so an insert or a removal moves one chunk's worth rather than the whole list, and a
 * range is found by two binary searches: over the chunks' last strings, then within one
 */
class SortedStrings {
  private chunks: string[][] = [];

  /** The chunk that holds `key`'s place: the first whose last string is not below it, or the last chunk */
  private chunkOf(key: string): number {
    let low = 0;
    let high = this.chunks.length;
    while (low < high) {
      const mid = (low + high) >>> 1;
      if (this.chunks[mid]!.at(-1)! < key) low = mid + 1;
      else high = mid;
    }
    return Math.min(low, this.chunks.length - 1);
  }

  insert(key: string): void {
    if (this.chunks.length === 0) { this.chunks.push([key]); return; }
    const index = this.chunkOf(key);
    const chunk = this.chunks[index]!;
    const at = lowerBound(chunk, key);
    if (chunk[at] === key) return;
    chunk.splice(at, 0, key);
    if (chunk.length > CHUNK) this.chunks.splice(index + 1, 0, chunk.splice(CHUNK / 2));
  }

  remove(key: string): void {
    if (this.chunks.length === 0) return;
    const index = this.chunkOf(key);
    const chunk = this.chunks[index]!;
    const at = lowerBound(chunk, key);
    if (chunk[at] !== key) return;
    chunk.splice(at, 1);
    if (chunk.length === 0) this.chunks.splice(index, 1);
  }

  /**
   * The strings in [low, high), one at a time, highest first with `reverse`: a function answering the next, or
   * undefined past the end
   */
  walk(low: string, high: string | undefined, reverse: boolean): () => string | undefined {
    const chunks = this.chunks;
    // A position: a chunk and an index in it
    let c: number;
    let i: number;
    if (!reverse) {
      c = chunks.length === 0 ? 0 : this.chunkOf(low);
      i = chunks.length === 0 ? 0 : lowerBound(chunks[c]!, low);
    } else if (high === undefined) {
      c = chunks.length - 1;
      i = c < 0 ? -1 : chunks[c]!.length - 1;
    } else {
      c = chunks.length === 0 ? -1 : this.chunkOf(high);
      i = c < 0 ? -1 : lowerBound(chunks[c]!, high) - 1;
    }
    return () => {
      for (;;) {
        if (c < 0 || c >= chunks.length) return undefined;
        const chunk = chunks[c]!;
        if (!reverse && i >= chunk.length) { c++; i = 0; continue; }
        if (reverse && i < 0) { c--; i = c >= 0 ? chunks[c]!.length - 1 : -1; continue; }
        const key = chunk[i]!;
        i += reverse ? -1 : 1;
        if (!reverse ? high !== undefined && key >= high : key < low) return (c = -1), undefined;
        return key;
      }
    };
  }
}

/**
 * A backend in memory, for tests and tooling and never the app: keys as byte strings in one sorted list, and each
 * key's committed versions, so a read snapshot answers from the version it began at however much commits after it, as
 * LMDB's does. A write unit holds its writes apart and its reads see them; it commits them as one new version, or
 * drops them. Versions no open snapshot can read are dropped as the last snapshot closes.
 */
export function createMemoryKVBackend(): KvBackend {
  /** Every key with a version, as a byte string (latin1, so string order is byte order), sorted */
  const keys = new SortedStrings();
  const versions = new Map<string, Version[]>();
  let version = 0;
  /** How many snapshots are open at each version */
  const open = new Map<number, number>();
  /** Keys written since versions were last pruned */
  const touched = new Set<string>();

  const toName = (bytes: Uint8Array) => byteString(bytes);
  const toBytes = (name: string) => Buffer.from(name, 'latin1');

  const at = (name: string, snapshot: number): Uint8Array | undefined => {
    const list = versions.get(name);
    if (!list) return undefined;
    for (let i = list.length - 1; i >= 0; i--) if (list[i]!.at <= snapshot) return list[i]!.bytes;
    return undefined;
  };

  /** Drops what no open snapshot can read: every version but each key's latest, and a key whose latest is a removal */
  const prune = () => {
    if (open.size > 0) return;
    for (const name of touched) {
      const list = versions.get(name);
      if (!list) continue;
      const latest = list[list.length - 1]!;
      if (latest.bytes === undefined) {
        versions.delete(name);
        keys.remove(name);
      } else if (list.length > 1) {
        versions.set(name, [latest]);
      }
    }
    touched.clear();
  };

  /** A unit's own writes: each key's bytes (undefined: removed), and the keys sorted, for its ranges */
  interface Pending { bytes: Map<string, Uint8Array | undefined>; names: SortedStrings }

  /**
   * Visits the committed keys in [low, high) as of a version, with a unit's own writes over them: two sorted runs
   * merged, read by index, so a range that stops early reads only what it visited
   */
  const scan = (snapshot: number, pending: Pending | undefined) =>
    (low: Uint8Array, high: Uint8Array | undefined, reverse: boolean, _values: boolean, visit: (key: Uint8Array, value: Uint8Array | undefined) => boolean) => {
      const lowName = toName(low);
      const highName = high && toName(high);
      const committed = keys.walk(lowName, highName, reverse);
      const written = pending ? pending.names.walk(lowName, highName, reverse) : () => undefined;
      const before = (a: string, b: string) => (reverse ? a > b : a < b);
      let nextCommitted = committed();
      let nextWritten = written();
      while (nextCommitted !== undefined || nextWritten !== undefined) {
        let name: string;
        if (nextWritten === undefined || (nextCommitted !== undefined && before(nextCommitted, nextWritten))) {
          name = nextCommitted!;
          nextCommitted = committed();
        } else {
          name = nextWritten;
          if (nextCommitted === nextWritten) nextCommitted = committed();
          nextWritten = written();
        }
        const bytes = pending?.bytes.has(name) ? pending.bytes.get(name) : at(name, snapshot);
        if (bytes === undefined) continue;
        if (!visit(toBytes(name), bytes)) return;
      }
    };

  let writing = false;
  return {
    read(fn) {
      const snapshot = version;
      open.set(snapshot, (open.get(snapshot) ?? 0) + 1);
      try {
        return fn({ get: (key) => at(toName(key), snapshot), scan: scan(snapshot, undefined) });
      } finally {
        const count = open.get(snapshot)! - 1;
        if (count === 0) open.delete(snapshot);
        else open.set(snapshot, count);
        prune();
      }
    },
    write(fn) {
      if (writing) throw new Error('A memory KV takes one write unit at a time');
      writing = true;
      const pending: Pending = { bytes: new Map(), names: new SortedStrings() };
      const hold = (name: string, bytes: Uint8Array | undefined) => {
        if (!pending.bytes.has(name)) pending.names.insert(name);
        pending.bytes.set(name, bytes);
      };
      const base = version;
      try {
        const result = fn({
          get: (key) => {
            const name = toName(key);
            return pending.bytes.has(name) ? pending.bytes.get(name) : at(name, base);
          },
          scan: scan(base, pending),
          // A copy: the encoder's bytes may be reused once this returns
          put: (key, value) => hold(toName(key), value.slice()),
          del: (key) => hold(toName(key), undefined),
        });
        if (pending.bytes.size > 0) {
          version++;
          for (const [name, bytes] of pending.bytes) {
            let list = versions.get(name);
            if (!list) {
              if (bytes === undefined) continue;
              versions.set(name, list = []);
              keys.insert(name);
            }
            list.push({ at: version, bytes });
            touched.add(name);
          }
          prune();
        }
        return result;
      } finally {
        writing = false;
      }
    },
  };
}

/** An ordered key-value store in memory: for tests and tooling, never the app */
export function createMemoryKV(): OrderedKV {
  return createOrderedKV(createMemoryKVBackend());
}
