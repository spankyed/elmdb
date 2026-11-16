// @ts-check
// The thin client: the binding's reads and writes taking and answering the protocol's generated types (`client.d.ts`),
// over the caller's own msgpack codec. Encoding and decoding values stays the caller's (`docs/architecture/design.md`,
// The boundary with apack); the client only picks the decode an answer needs, from what the answer says of itself.
'use strict';

/**
 * Whether an answer decodes exactly with maps as objects: its own `plainObjects`, or the one of the answer it wraps
 * @param {{ kind: string, plainObjects?: boolean, answer?: any }} answer
 * @returns {boolean}
 */
function plain(answer) {
  if (answer.kind === 'visited' || answer.kind === 'explained') return plain(answer.answer);
  return answer.plainObjects !== false;
}

/**
 * A value decoded with maps as `Map`s, each map made an object with no prototype, which holds a key `__proto__` as its
 * own. Every key is a string: elmdb refuses a stored map keyed by anything else (`format.md`, values)
 * @param {unknown} value
 * @returns {unknown}
 */
function objects(value) {
  if (Array.isArray(value)) return value.map(objects);
  if (!(value instanceof Map)) return value;
  const object = Object.create(null);
  for (const [key, item] of value) object[key] = objects(item);
  return object;
}

/**
 * A call's answers: decoded with maps as objects where every answer says that is exact, and otherwise decoded with
 * maps as `Map`s and made objects that hold `__proto__` as a key
 * @param {import('./client').Codec} codec
 * @param {Buffer} bytes
 */
function answers(codec, bytes) {
  const decoded = /** @type {any[]} */ (codec.unpack(bytes));
  return decoded.every(plain) ? decoded : objects(codec.unpackMaps(bytes));
}

class Client {
  /**
   * @param {import('./index').Database} db
   * @param {import('./client').Codec} codec
   */
  constructor(db, codec) {
    this.db = db;
    this.codec = codec;
  }

  /** @param {import('./index').Reads} reads */
  read(reads) {
    return answers(this.codec, this.db.read(this.codec.pack(reads)));
  }

  /** @param {import('./index').Reads} reads */
  async readAsync(reads) {
    return answers(this.codec, await this.db.readAsync(this.codec.pack(reads)));
  }

  /**
   * @param {import('./protocol/Batch').Batch} batch
   * @param {boolean} [durable]
   */
  write(batch, durable) {
    return this.codec.unpack(this.db.write(this.codec.pack(batch), durable));
  }

  begin() {
    return new ClientUnit(this.db.begin(), this.codec);
  }
}

class ClientUnit {
  /**
   * @param {import('./index').Unit} unit
   * @param {import('./client').Codec} codec
   */
  constructor(unit, codec) {
    this.unit = unit;
    this.codec = codec;
  }

  /** @param {import('./index').Reads} reads */
  read(reads) {
    return answers(this.codec, this.unit.read(this.codec.pack(reads)));
  }

  /** @param {import('./protocol/Batch').Batch} batch */
  write(batch) {
    return this.codec.unpack(this.unit.write(this.codec.pack(batch)));
  }

  pendingChanges() {
    return this.codec.unpack(this.unit.pendingChanges());
  }

  /** @param {boolean} [durable] */
  commit(durable) {
    return this.codec.unpack(this.unit.commit(durable));
  }

  abort() {
    this.unit.abort();
  }
}

module.exports = { Client, ClientUnit };
