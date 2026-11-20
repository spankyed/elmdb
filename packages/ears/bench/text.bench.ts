// The text index as a chat log grows: a message write's commit and a fuzzy search, at 1,000, 10,000 and 50,000
// messages, each on a database of its own, against which elmdb's docs/roadmap/text-index-at-scale.md decides what to
// build: a write's growth beside the store's own (a message with no text), and a fuzzy search's beside a common word's
// ranking. Run with `npm run bench -w @apackdev/ears`, in the built engine, synced as the app's is; the readings are in
// elmdb's docs/evidence/next-phase-6.md.
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { afterAll, bench, describe } from 'vitest';

// Specifiers held in variables, so the typecheck (which runs before any build) reads the types from source
const DIST = '../dist/index.js';
const DIST_LMDB = '../dist/lmdb/index.js';
const { createEarsEngine } = await import(/* @vite-ignore */ DIST) as typeof import('../src/index.ts');
const { openDatabase } = await import(/* @vite-ignore */ DIST_LMDB) as typeof import('../src/lmdb/index.ts');

const SIZES = [1_000, 10_000, 50_000] as const;
const WORDS_PER_MESSAGE = 30;
const VOCABULARY = 5_000;

/** A word of the vocabulary: the commonest few are a chat's own (`the`, `and`, …), the rest made up and five or more letters */
const COMMON = ['the', 'and', 'to', 'a', 'of', 'is', 'it', 'that', 'in', 'you', 'this', 'for', 'with', 'on', 'we', 'can', 'be', 'not', 'but', 'have'];
const word = (n: number) => COMMON[n] ?? `w${n.toString(36).padStart(4, 'q')}x`;

/** A deterministic generator, so every size draws the same messages up to its count */
function random(seed: number) {
  let state = seed >>> 0;
  return () => {
    state = (state * 1_664_525 + 1_013_904_223) >>> 0;
    return state / 2 ** 32;
  };
}

/**
 * A message of chat-like text: words drawn by a Zipf-like rank, so a few are in nearly every message and most are rare,
 * and two names of its own — an id, a path, a symbol, as a chat about code holds — so the dictionary grows with the log
 */
let named = 0;
function messageText(next: () => number): string {
  const words: string[] = [];
  for (let i = 0; i < WORDS_PER_MESSAGE; i++) words.push(word(Math.floor(VOCABULARY ** next()) - 1));
  for (let i = 0; i < 2; i++) words.push(`id${(named++).toString(36).padStart(5, 'k')}`);
  return words.join(' ');
}

const dirs: string[] = [];
const databases: Array<{ close(): void }> = [];
afterAll(() => {
  for (const database of databases) database.close();
  for (const dir of dirs) fs.rmSync(dir, { recursive: true, force: true });
});

/** A database of `count` messages with a text index on their text, loaded in units of a thousand: one unit of them all
 * would outgrow the map, which a transaction can't grow while it is open */
function chatLog(count: number) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `ears-text-bench-${count}-`));
  dirs.push(dir);
  const database = openDatabase({
    path: path.join(dir, 'ears-db'),
    schema: { types: { Message: { required: [], search: { fields: { text: 1 } } } }, relations: {}, roles: [] },
  });
  databases.push(database);
  const engine = createEarsEngine({ database });
  const next = random(1);
  for (let done = 0; done < count; done += 1_000) {
    engine.transact(() => {
      for (let i = done; i < Math.min(count, done + 1_000); i++) engine.create('Message', { text: messageText(next) });
    });
  }
  return { engine, next };
}

for (const count of SIZES) {
  describe(`text, ${count.toLocaleString('en')} messages`, () => {
    const { engine, next } = chatLog(count);
    // The common words are in nearly every message, so each write extends their posting lists. A message with no text
    // is the store's own share: what a write's time grows by as the database does, the text index aside
    bench(`a message write at ${count.toLocaleString('en')}`, () => {
      engine.create('Message', { text: messageText(next) });
    });
    bench(`a message with no text at ${count.toLocaleString('en')}`, () => {
      engine.create('Message', {});
    });
    // A fuzzy search walks every term of the dictionary, which grows with the log: a rare word misspelled matches its
    // neighbours an edit away and ranks few, so its time is the walk's — the gate's reading. A common word, in
    // thousands of messages, ranks each before keeping twenty: exactly or misspelled, its time is ranking's, which
    // the gate leaves to block-max scores (text-index-at-scale.md, When)
    const rare = word(3_000).replace(/x$/, 'y');
    bench(`a fuzzy search at ${count.toLocaleString('en')}`, () => {
      engine.query('Message').search(rare).limit(20).ids();
    });
    const common = word(40);
    bench(`a common word misspelled at ${count.toLocaleString('en')}`, () => {
      engine.query('Message').search(common.replace(/x$/, 'y')).limit(20).ids();
    });
    bench(`a common word searched exactly at ${count.toLocaleString('en')}`, () => {
      engine.query('Message').search(common, { fuzzy: false }).limit(20).ids();
    });
  });
}
