// @ts-check
// Clean-room elmdb v2 zero-dependency binary wire batch encoder (`elmdb-wire` EWB2).
// Speaks directly to `Database.executeWireBatch` with zero MsgPack or JSON overhead.
'use strict';

const TEXT_ENCODER = new TextEncoder();
const TYPED_ARRAY_PROTO = Object.getPrototypeOf(Uint8Array.prototype);
const TYPED_ARRAY_TAG_GETTER = Object.getOwnPropertyDescriptor(TYPED_ARRAY_PROTO, Symbol.toStringTag)?.get;
const TYPED_ARRAY_BUFFER_GETTER = Object.getOwnPropertyDescriptor(TYPED_ARRAY_PROTO, 'buffer')?.get;
const TYPED_ARRAY_BYTE_OFFSET_GETTER = Object.getOwnPropertyDescriptor(TYPED_ARRAY_PROTO, 'byteOffset')?.get;
const TYPED_ARRAY_BYTE_LENGTH_GETTER = Object.getOwnPropertyDescriptor(TYPED_ARRAY_PROTO, 'byteLength')?.get;
const DATE_GET_TIME = Date.prototype.getTime;

/**
 * Brand-checks a genuine `Uint8Array` (including Node `Buffer` and cross-realm `Uint8Array`)
 * via `%TypedArray%.prototype[Symbol.toStringTag]`, rejecting prototype-spoofed objects.
 * @param {unknown} v
 * @returns {Uint8Array | null}
 */
function asRealUint8Array(v) {
  if (
    v === null ||
    typeof v !== 'object' ||
    !TYPED_ARRAY_TAG_GETTER ||
    !TYPED_ARRAY_BUFFER_GETTER ||
    !TYPED_ARRAY_BYTE_OFFSET_GETTER ||
    !TYPED_ARRAY_BYTE_LENGTH_GETTER
  ) {
    return null;
  }
  try {
    if (TYPED_ARRAY_TAG_GETTER.call(v) !== 'Uint8Array') return null;
    const buffer = /** @type {ArrayBufferLike} */ (TYPED_ARRAY_BUFFER_GETTER.call(v));
    const byteOffset = /** @type {number} */ (TYPED_ARRAY_BYTE_OFFSET_GETTER.call(v));
    const byteLength = /** @type {number} */ (TYPED_ARRAY_BYTE_LENGTH_GETTER.call(v));
    return new Uint8Array(buffer, byteOffset, byteLength);
  } catch {
    return null;
  }
}

/**
 * Brand-checks a genuine `Date` (including cross-realm `Date` instances) via intrinsic
 * `Date.prototype.getTime`, returning `null` for non-Dates and prototype-spoofed objects.
 * @param {unknown} v
 * @returns {number | null}
 */
function asRealDateMs(v) {
  if (v === null || typeof v !== 'object') return null;
  try {
    return DATE_GET_TIME.call(v);
  } catch {
    return null;
  }
}

/**
 * Returns true if `s` contains any unpaired UTF-16 surrogate code unit (`0xD800..=0xDFFF`),
 * which `TextEncoder` would silently replace with U+FFFD and could collide distinct JS keys.
 * @param {string} s
 * @returns {boolean}
 */
function hasLoneSurrogate(s) {
  for (let i = 0; i < s.length; i++) {
    const code = s.charCodeAt(i);
    if (code >= 0xd800 && code <= 0xdbff) {
      const next = i + 1 < s.length ? s.charCodeAt(i + 1) : 0;
      if (next >= 0xdc00 && next <= 0xdfff) {
        i++;
      } else {
        return true;
      }
    } else if (code >= 0xdc00 && code <= 0xdfff) {
      return true;
    }
  }
  return false;
}

/** @type {WeakMap<BatchSlot, WireBatchBuilder>} */
const SLOT_OWNERS = new WeakMap();

/** Branded intra-batch reference returned by `WireBatchBuilder.create(...)`. */
class BatchSlot {
  /** @param {number} index */
  constructor(index) {
    if (typeof index !== 'number' || !Number.isInteger(index) || index < 0 || index > 0xffffffff) {
      throw new RangeError(`BatchSlot index must be an integer in [0, 4294967295], got ${String(index)}`);
    }
    /** @readonly */
    this.slot = index;
    Object.freeze(this);
  }
}

/** @param {unknown} v */
function isPlainObject(v) {
  if (v === null || typeof v !== 'object' || Array.isArray(v)) return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
}

class WireWriter {
  constructor(initialCapacity = 256) {
    this.buf = new Uint8Array(initialCapacity);
    this.view = new DataView(this.buf.buffer);
    this.len = 0;
  }

  /** @param {number} extra */
  ensure(extra) {
    if (this.len + extra <= this.buf.length) return;
    let next = this.buf.length * 2;
    while (next < this.len + extra) next *= 2;
    const grown = new Uint8Array(next);
    grown.set(this.buf.subarray(0, this.len));
    this.buf = grown;
    this.view = new DataView(this.buf.buffer);
  }

  /** @param {number} b */
  u8(b) {
    this.ensure(1);
    this.buf[this.len++] = b & 0xff;
  }

  /** @param {number} n */
  u32(n) {
    this.ensure(4);
    this.view.setUint32(this.len, n >>> 0, true);
    this.len += 4;
  }

  /** @param {bigint} n */
  i64(n) {
    this.ensure(8);
    this.view.setBigInt64(this.len, n, true);
    this.len += 8;
  }

  /** @param {number} n */
  f64(n) {
    this.ensure(8);
    this.view.setFloat64(this.len, n, true);
    this.len += 8;
  }

  /** @param {Uint8Array} bytes */
  raw(bytes) {
    this.ensure(bytes.length);
    this.buf.set(bytes, this.len);
    this.len += bytes.length;
  }

  /** @param {string} s */
  str(s) {
    if (typeof s !== 'string') {
      throw new TypeError(`Expected string, got ${typeof s}`);
    }
    const bytes = TEXT_ENCODER.encode(s);
    this.u32(bytes.length);
    this.raw(bytes);
  }

  /** @param {string} s @param {string} label */
  ident(s, label) {
    if (typeof s !== 'string') {
      throw new TypeError(`Expected string for ${label}, got ${typeof s}`);
    }
    if (hasLoneSurrogate(s)) {
      throw new TypeError(`${label} contains an unpaired UTF-16 surrogate`);
    }
    const bytes = TEXT_ENCODER.encode(s);
    this.u32(bytes.length);
    this.raw(bytes);
  }

  /** @param {string | BatchSlot} target @param {WireBatchBuilder} owner */
  target(target, owner) {
    if (target instanceof BatchSlot) {
      if (SLOT_OWNERS.get(target) !== owner) {
        throw new TypeError('BatchSlot does not belong to this WireBatchBuilder');
      }
      this.u8(1);
      this.u32(target.slot);
    } else if (typeof target === 'string') {
      this.u8(0);
      this.ident(target, 'Entity ID');
    } else {
      throw new TypeError('Entity target must be a string external ID or a BatchSlot');
    }
  }

  /** @param {unknown} value @param {number} depth */
  value(value, depth = 0) {
    if (depth > 64) {
      throw new RangeError('Value nesting depth exceeds 64');
    }
    if (value === null || value === undefined) {
      this.u8(0); // Null
      return;
    }
    if (typeof value === 'boolean') {
      this.u8(value ? 2 : 1);
      return;
    }
    if (typeof value === 'bigint') {
      if (value < -9223372036854775808n || value > 9223372036854775807n) {
        throw new RangeError('BigInt outside signed 64-bit range');
      }
      this.u8(3); // Int
      this.i64(value);
      return;
    }
    if (typeof value === 'number') {
      if (!Number.isFinite(value)) {
        throw new RangeError('Non-finite numbers (NaN, Infinity) are not permitted');
      }
      const norm = value === 0 ? 0 : value;
      if (Number.isInteger(norm) && Number.isSafeInteger(norm)) {
        this.u8(3); // Int
        this.i64(BigInt(norm));
      } else {
        this.u8(4); // Float
        this.f64(norm);
      }
      return;
    }
    if (typeof value === 'string') {
      this.u8(8); // String
      this.str(value);
      return;
    }
    if (value instanceof BatchSlot) {
      throw new TypeError('BatchSlot cannot be passed as a field value; use link() or an explicit externalId');
    }
    const dateMs = asRealDateMs(value);
    if (dateMs !== null) {
      if (!Number.isFinite(dateMs)) {
        throw new RangeError('Invalid Date timestamp');
      }
      this.u8(5); // Timestamp
      this.i64(BigInt(Math.trunc(dateMs)));
      this.u32(0);
      return;
    }
    const realBytes = asRealUint8Array(value);
    if (realBytes !== null) {
      this.u8(9); // Bytes
      this.u32(realBytes.byteLength);
      this.raw(realBytes);
      return;
    }
    if (Array.isArray(value)) {
      this.u8(10); // Array
      const lenPos = this.len;
      this.u32(0);
      this.u32(value.length);
      const start = this.len;
      for (let i = 0; i < value.length; i++) {
        this.value(value[i], depth + 1);
      }
      this.view.setUint32(lenPos, (this.len - start + 4) >>> 0, true);
      return;
    }
    if (isPlainObject(value)) {
      this.u8(11); // Object
      const lenPos = this.len;
      this.u32(0);
      /** @type {Array<[Uint8Array, unknown]>} */
      const entries = [];
      const obj = /** @type {Record<string, unknown>} */ (value);
      for (const key of Object.keys(obj)) {
        if (key === '__proto__') {
          throw new TypeError("Reserved key '__proto__' is not permitted");
        }
        if (hasLoneSurrogate(key)) {
          throw new TypeError('Object key contains an unpaired UTF-16 surrogate');
        }
        const v = obj[key];
        if (v !== undefined) {
          entries.push([TEXT_ENCODER.encode(key), v]);
        }
      }
      // Strictly sort keys in UTF-8 byte order to match Rust `str::cmp`.
      entries.sort((a, b) => compareBytes(a[0], b[0]));
      for (let i = 1; i < entries.length; i++) {
        if (compareBytes(entries[i - 1][0], entries[i][0]) === 0) {
          throw new TypeError('Object contains duplicate keys after UTF-8 encoding');
        }
      }
      this.u32(entries.length);
      const start = this.len;
      for (let i = 0; i < entries.length; i++) {
        const [kBytes, v] = entries[i];
        this.u32(kBytes.length);
        this.raw(kBytes);
        this.value(v, depth + 1);
      }
      this.view.setUint32(lenPos, (this.len - start + 4) >>> 0, true);
      return;
    }
    throw new TypeError(`Unsupported value type: ${Object.prototype.toString.call(value)}`);
  }

  /** @param {Record<string, unknown>} fields */
  fieldPairs(fields) {
    if (!isPlainObject(fields)) {
      throw new TypeError('Fields must be a plain object');
    }
    /** @type {Array<[string, unknown]>} */
    const pairs = [];
    for (const key of Object.keys(fields)) {
      if (key === '__proto__') {
        throw new TypeError("Reserved field name '__proto__' is not permitted");
      }
      if (hasLoneSurrogate(key)) {
        throw new TypeError('Field name contains an unpaired UTF-16 surrogate');
      }
      const val = fields[key];
      if (val !== undefined) pairs.push([key, val]);
    }
    this.u32(pairs.length);
    for (let i = 0; i < pairs.length; i++) {
      this.str(pairs[i][0]);
      this.value(pairs[i][1], 0);
    }
  }

  finish() {
    return this.buf.subarray(0, this.len);
  }
}

/**
 * Unsigned lexicographical byte comparison (matches Rust `str::cmp` / UTF-8 order).
 * @param {Uint8Array} a
 * @param {Uint8Array} b
 * @returns {number}
 */
function compareBytes(a, b) {
  const min = Math.min(a.length, b.length);
  for (let i = 0; i < min; i++) {
    if (a[i] !== b[i]) return a[i] - b[i];
  }
  return a.length - b.length;
}

/**
 * Builder for constructing an atomic `elmdb-wire` v2 binary write batch.
 * Supports intra-batch references via `BatchSlot` handles returned by `create(...)`.
 */
class WireBatchBuilder {
  constructor() {
    /** @type {Array<any>} */
    this._ops = [];
    this._nextSlot = 0;
  }

  /**
   * @param {string} typeName
   * @param {Record<string, unknown>} fields
   * @param {string} [externalId]
   * @returns {BatchSlot}
   */
  create(typeName, fields, externalId) {
    const slot = new BatchSlot(this._nextSlot++);
    SLOT_OWNERS.set(slot, this);
    this._ops.push({ tag: 1, typeName, externalId, fields: fields === undefined ? {} : fields });
    return slot;
  }

  /**
   * @param {string | BatchSlot} target
   * @param {Record<string, unknown>} fields
   */
  patch(target, fields) {
    this._ops.push({ tag: 2, target, fields: fields === undefined ? {} : fields });
    return this;
  }

  /**
   * @param {string | BatchSlot} target
   * @param {string[]} fields
   */
  unset(target, fields) {
    if (!Array.isArray(fields)) {
      throw new TypeError('unset fields must be an array of field names');
    }
    this._ops.push({ tag: 3, target, fields });
    return this;
  }

  /** @param {string | BatchSlot} target */
  trash(target) {
    this._ops.push({ tag: 4, target });
    return this;
  }

  /** @param {string | BatchSlot} target */
  restore(target) {
    this._ops.push({ tag: 5, target });
    return this;
  }

  /** @param {string | BatchSlot} target */
  destroy(target) {
    this._ops.push({ tag: 6, target });
    return this;
  }

  /**
   * @param {string | BatchSlot} source
   * @param {string} relKind
   * @param {string | BatchSlot} target
   * @param {{ weight?: number, info?: unknown }} [opts]
   */
  link(source, relKind, target, opts) {
    this._ops.push({ tag: 7, source, relKind, target, weight: opts?.weight, info: opts?.info });
    return this;
  }

  /**
   * @param {string | BatchSlot} source
   * @param {string} [relKind]
   * @param {string | BatchSlot} [target]
   */
  unlink(source, relKind, target) {
    this._ops.push({ tag: 8, source, relKind, target });
    return this;
  }

  /** @returns {number} */
  get length() {
    return this._ops.length;
  }

  /**
   * Encodes all queued ops into a contiguous `elmdb-wire` (`EWB2`) `Uint8Array`.
   * @param {number} [returnFlags]
   * @returns {Uint8Array}
   */
  encode(returnFlags = 0) {
    if (typeof returnFlags !== 'number' || !Number.isInteger(returnFlags) || returnFlags < 0 || returnFlags > 0x07) {
      throw new RangeError(`returnFlags must be an integer in [0, 7], got ${String(returnFlags)}`);
    }
    const w = new WireWriter(64 + this._ops.length * 32);
    // Magic "EWB2"
    w.u8(0x45);
    w.u8(0x57);
    w.u8(0x42);
    w.u8(0x32);
    w.u8(returnFlags);
    w.u32(this._ops.length);

    for (let i = 0; i < this._ops.length; i++) {
      const op = this._ops[i];
      w.u8(op.tag);
      switch (op.tag) {
        case 1:
          w.ident(op.typeName, 'Type name');
          if (op.externalId !== undefined && op.externalId !== null) {
            w.u8(1);
            w.ident(op.externalId, 'Entity ID');
          } else {
            w.u8(0);
          }
          w.fieldPairs(op.fields);
          break;
        case 2:
          w.target(op.target, this);
          w.fieldPairs(op.fields);
          break;
        case 3:
          w.target(op.target, this);
          w.u32(op.fields.length);
          for (const f of op.fields) w.ident(f, 'Field name');
          break;
        case 4:
        case 5:
        case 6:
          w.target(op.target, this);
          break;
        case 7:
          w.target(op.source, this);
          w.ident(op.relKind, 'Relation kind');
          w.target(op.target, this);
          if (op.weight !== undefined && op.weight !== null) {
            if (typeof op.weight !== 'number' || !Number.isFinite(op.weight)) {
              throw new RangeError('Link weight must be a finite number');
            }
            w.u8(1);
            w.f64(op.weight);
          } else {
            w.u8(0);
          }
          if (op.info !== undefined) {
            w.u8(1);
            w.value(op.info, 0);
          } else {
            w.u8(0);
          }
          break;
        case 8:
          w.target(op.source, this);
          if (op.relKind !== undefined && op.relKind !== null) {
            w.u8(1);
            w.ident(op.relKind, 'Relation kind');
          } else {
            w.u8(0);
          }
          if (op.target !== undefined && op.target !== null) {
            w.u8(1);
            w.target(op.target, this);
          } else {
            w.u8(0);
          }
          break;
      }
    }
    return w.finish();
  }
}

/**
 * Convenience helper: constructs and encodes a wire batch via callback.
 * Rejects async callbacks (Promises/thenables) synchronously to prevent misused batches.
 * @param {(b: WireBatchBuilder) => void} fn
 * @returns {Uint8Array}
 */
function buildWireBatch(fn) {
  const b = new WireBatchBuilder();
  const ret = fn(b);
  if (ret !== null && (typeof ret === 'object' || typeof ret === 'function') && typeof /** @type {any} */ (ret).then === 'function') {
    throw new TypeError('Batch callbacks must be synchronous (Promises/async callbacks are not allowed)');
  }
  return b.encode();
}

module.exports = { BatchSlot, WireBatchBuilder, buildWireBatch };
