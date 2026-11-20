// Text search through the builder: a type's `search` declaration as elmdb's text index, `search` ranked and combined
// with the steps around it, `after` paging, and `hits` with highlights and picked fields; and its `substrings` as
// elmdb's trigram index, which `containing` and `matching` read. How elmdb matches and ranks is its own specs'
// (elmdb/crates/elmdb/src/engine/text/tests.rs, engine/tests/trigram.rs); these hold the builder to sending what it means.
import { describe, expect, it } from 'vitest';
import type { EARS } from '../../src/index.ts';
import { freshEngine } from './engine-under-test.ts';

type Id = EARS.EntityId;

const engine = ({ freeze = false } = {}) => freshEngine({
  freeze,
  types: ['Message', 'Thread', 'Plain'],
  relationKinds: ['in'],
  roles: [],
  rowRules: {
    Message: { required: [], search: { fields: { text: 1, sender: 2 } }, substrings: ['text'] },
    Thread: { required: [], search: { fields: { title: 1 }, stem: 'en' } },
  },
});

describe('search', () => {
  it('ranks matches, most relevant first, and combines with the steps around it', () => {
    const e = engine();
    const a = e.create('Message', { text: 'the database is fast' }).id();
    const b = e.create('Message', { text: 'a database engine on a database' }).id();
    e.create('Message', { text: 'cooking pasta' });
    expect(e.query('Message').search('database').ids()).toEqual([b, a]);
    expect(e.query('Message').search('database -fast').ids()).toEqual([b]);
    expect(e.query('Message').search('datab', { prefix: true }).count()).toBe(2);
    expect(e.query('Message').search('databse').ids()).toEqual([b, a]);
    expect(e.query('Message').search('databse', { fuzzy: false }).ids()).toEqual([]);
    const thread = e.create('Thread', { title: 'Releases' }).link('in', a).id();
    expect(e.query.ids([thread]).out('in', 'Message').search('database').ids()).toEqual([a]);
    expect(e.query('Thread').search('release').ids()).toEqual([thread]);
  });

  it('weighs a field, restricts to fields and ranks newer first with recency', () => {
    const e = engine();
    // Documents of one length at their weights, so the weight is what differs
    const inText = e.create('Message', { text: 'ada', sender: 'bob', at: 1000 }).id();
    const inSender = e.create('Message', { text: 'hello', sender: 'ada', at: 90_000 }).id();
    expect(e.query('Message').search('ada').ids()).toEqual([inSender, inText]);
    expect(e.query('Message').search('ada', { fields: ['text'] }).ids()).toEqual([inText]);
    expect(e.query('Message').search('ada', { fields: ['text', 'sender'], recency: { field: 'at', halfLife: 10_000, now: 100_000 } }).first())
      .toBe(inSender);
  });

  it('pages after the last id of a page', () => {
    const e = engine();
    const ids = ['one note', 'two note', 'three note'].map((text) => e.create('Message', { text }).id());
    const first = e.query('Message').search('note').limit(2).ids();
    expect(first).toEqual(ids.slice(0, 2));
    expect(e.query('Message').search('note').after(first[1]!).ids()).toEqual([ids[2]]);
  });

  it('answers hits with scores, matched words, highlights in UTF-16 units and picked fields', () => {
    const e = engine();
    const id = e.create('Message', { text: '😀 Database!', sender: 'ada' }).id();
    const [hit] = e.query('Message').search('database').hits({ fields: ['sender'], highlights: true });
    expect(hit).toEqual({
      id, score: expect.any(Number), terms: ['database'], highlights: { text: [[0, 3, 11]] }, row: { id, sender: 'ada' },
    });
    expect(e.query('Message').search('database').hits()[0]).not.toHaveProperty('highlights');
  });

  it("freezes a hit's picked field values where reads are frozen, as a row's are", () => {
    const e = engine({ freeze: true });
    e.create('Message', { text: 'database', sender: 'ada', meta: { n: 1 } });
    const [hit] = e.query('Message').search('database').hits({ fields: ['meta'] });
    expect(Object.isFrozen(hit!.row!.meta)).toBe(true);
  });

  it('refuses a type with no text index, and hits for a query with none or a function after it', () => {
    const e = engine();
    e.create('Message', { text: 'x' });
    expect(() => e.query('Plain').search('x').ids()).toThrow(expect.objectContaining({ code: 'UNKNOWN_INDEX' }));
    expect(() => e.query('Message').hits()).toThrow(expect.objectContaining({ code: 'NO_SEARCH' }));
    expect(() => e.query('Message').search('x').where('text', (v: unknown) => v === 'x').hits())
      .toThrow(expect.objectContaining({ code: 'NO_SEARCH' }));
    expect(e.query('Message').where('text', (v: unknown) => v === 'x').search('x').hits()).toHaveLength(1);
  });

  it('keeps the index through writes, and a destroyed entity is no match', () => {
    const e = engine();
    const id = e.create('Message', { text: 'alpha' }).id() as Id;
    e.write(id).set('text', 'beta');
    expect(e.query('Message').search('alpha').ids()).toEqual([]);
    expect(e.query('Message').search('beta').ids()).toEqual([id]);
    e.write(id).destroy();
    expect(e.query('Message').search('beta').ids()).toEqual([]);
  });
});

describe('containing and matching', () => {
  it('find text as it is written, in the order the query had, among what the steps before reached', () => {
    const e = engine();
    const timeout = e.create('Message', { text: 'Error: ETIMEDOUT after 30000ms' }).id();
    const typeError = e.create('Message', { text: "TypeError: Cannot read properties of undefined (reading 'seq')" }).id();
    const commit = e.create('Message', { text: 'fixed in a1b6603e5a' }).id();
    e.create('Message', { text: 'all good' });
    expect(e.query('Message').containing('ETIMEDOUT').ids()).toEqual([timeout]);
    expect(e.query('Message').containing('error:').ids()).toEqual([]);
    expect(e.query('Message').containing('error:', { caseInsensitive: true }).ids()).toEqual([timeout, typeError]);
    expect(e.query('Message').matching(String.raw`[0-9a-f]{10}`).ids()).toEqual([commit]);
    expect(e.query('Message').matching('(?i)^error').ids()).toEqual([timeout]);
    expect(e.query('Message').orderBy('text', 'desc').containing('Error').ids()).toEqual([typeError, timeout]);
    const thread = e.create('Thread', { title: 'Failures' }).link('in', typeError).id();
    expect(e.query.ids([thread]).out('in', 'Message').containing('Error').ids()).toEqual([typeError]);
  });

  it('refuse a type with no trigram index, and a pattern the syntax does not hold, naming it', () => {
    const e = engine();
    e.create('Message', { text: 'x' });
    expect(() => e.query('Plain').containing('x').ids()).toThrow(expect.objectContaining({ code: 'UNKNOWN_INDEX' }));
    expect(() => e.query('Message').matching('x(?=y)').ids())
      .toThrow(expect.objectContaining({ code: 'PATTERN_INVALID', message: expect.stringContaining('x(?=y)') }));
  });

  it('follow writes', () => {
    const e = engine();
    const id = e.create('Message', { text: 'alpha-1' }).id() as Id;
    e.write(id).set('text', 'beta-2');
    expect(e.query('Message').containing('alpha-1').ids()).toEqual([]);
    expect(e.query('Message').containing('beta-2').ids()).toEqual([id]);
    e.write(id).destroy();
    expect(e.query('Message').containing('beta-2').ids()).toEqual([]);
  });
});
