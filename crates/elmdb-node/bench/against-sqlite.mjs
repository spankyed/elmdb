// One workload on elmdb and on SQLite (Node's built-in `node:sqlite`, FTS5 and WAL), a million notes unless told
// otherwise: `node crates/elmdb-node/bench/against-sqlite.mjs [--json] [--notes N] [--dir PATH]` after `npm run build`
// (`docs/architecture/testing.md`, Against SQLite). Each note has a title in a text index and a number and a status in
// value indexes. It reads what a note costs in bytes, what a create's commit costs at a twentieth of the notes and at
// all of them, and what a search, an ordered read, a range, a page, a check and a rebuild cost at all of them. Neither
// engine syncs. The two answer each read alike before any is timed.
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { tmpdir, cpus, totalmem, loadavg, platform, arch } from 'node:os';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import { Packr } from 'msgpackr';

const require = createRequire(import.meta.url);
const { Database } = require('../index.js');
const { DatabaseSync } = require('node:sqlite');
const packr = new Packr({ useRecords: false, mapsAsObjects: true });

const USAGE = 'usage: against-sqlite.mjs [--json] [--notes N] [--dir PATH] [--require-text]';
const args = process.argv.slice(2);
let json = false;
let NOTES = 1_000_000;
let parent = tmpdir();
// Refuses a SQLite built without FTS5, which answers no search for elmdb's to be held to
let requireText = false;
while (args.length) {
  const arg = args.shift();
  if (arg === '--json') json = true;
  else if (arg === '--notes') NOTES = Number(args.shift());
  else if (arg === '--dir') parent = args.shift();
  else if (arg === '--require-text') requireText = true;
  else assert.fail(USAGE);
}
// A range's last 500 and a first checkpoint of a twentieth need this many
assert.ok(Number.isSafeInteger(NOTES) && NOTES >= 2000, 'notes must be a whole number, 2000 or more');
assert.ok(parent, USAGE);

const WORDS = ['alpha', 'bravo', 'charlie', 'delta', 'echo', 'foxtrot', 'golf', 'hotel', 'india', 'juliet', 'kilo', 'lima'];
const UNIT = 5000;
const COMMITS = 41;
const id = (i) => `Note-${String(i).padStart(7, '0')}`;
// Six terms a note: three every note holds, two a twelfth hold, and its own number, which no other holds
const note = (i) => ({
  id: id(i),
  title: `the ${WORDS[i % 12]} ${WORDS[(i * 7) % 12]} note number ${i}`,
  n: (i * 7919) % NOTES,
  status: i % 2 ? 'open' : 'done',
});

// Ten more notes, of ten words each, holding a word no other does from once to ten times: equally long, so either
// engine's ranking puts the one holding it most first, whatever its formula's constants
const RANKED = Array.from({ length: 10 }, (_, k) => ({
  id: `Note-r${k}`,
  title: [...Array(k + 1).fill('quokka'), ...Array(9 - k).fill('wombat')].join(' '),
  n: NOTES + k,
  status: 'open',
}));
const RANKED_BEST = RANKED.map((one) => one.id).reverse();

const ms = (from) => Number(process.hrtime.bigint() - from) / 1e6;
const median = (samples) => [...samples].sort((a, b) => a - b)[Math.floor(samples.length / 2)];
const round = (value, places = 3) => Number(value.toFixed(places));
const seconds = (work) => {
  const from = process.hrtime.bigint();
  work();
  return round(ms(from) / 1000, 2);
};
// The median of a read's time in milliseconds, and its last answer
function timed(runs, work) {
  const samples = [];
  let answer;
  for (let i = 0; i < runs; i++) {
    const from = process.hrtime.bigint();
    answer = work();
    samples.push(ms(from));
  }
  return { ms: round(median(samples)), answer };
}
const progress = (text) => process.stderr.write(`${text}\n`);

const root = mkdtempSync(join(parent, 'elmdb-against-sqlite-'));

// elmdb through the binding, as a program calls it
function elmdb() {
  const dir = join(root, 'elmdb');
  const schema = {
    $schema: 'https://elmdb.dev/schema/1.json',
    types: {
      Note: {
        fields: { title: { type: 'string' }, n: { type: 'number' }, status: { type: 'enum', values: ['open', 'done'] } },
        indexes: {
          status: { kind: 'value', fields: ['status'] },
          n: { kind: 'value', fields: ['n'] },
          text: { kind: 'text', fields: { title: {} } },
        },
      },
    },
    relations: {},
  };
  const db = Database.open(dir, { schema, syncs: false });
  const create = ({ id, ...fields }) => ({ op: 'create', type: 'Note', id, fields });
  const query = (steps, terminal, extra = {}) => {
    const plan = { start: { from: 'types', types: ['Note'] }, steps, terminal };
    return packr.unpack(db.readSync(packr.pack({ reads: [{ read: 'query', plan, ...extra }] })))[0];
  };
  const search = (text, fuzzy) => ({ step: 'search', query: text, ...(fuzzy ? {} : { options: { fuzzy: false } }) });
  return {
    text: true,
    load: (notes) => db.write(packr.pack({ ops: notes.map(create) })),
    create: (one) => db.write(packr.pack({ ops: [create(one)] })),
    settle: () => db.waitForIndexBuilds(),
    best: (text, fuzzy = false) => query([search(text, fuzzy), { step: 'limit', n: 10 }], { answer: 'ids' }).ids,
    // How many entities the plan visited to answer the best ten
    visited: (text) => query([search(text, false), { step: 'limit', n: 10 }], { answer: 'ids' }, { explain: true }).visited,
    count: (text) => query([search(text, false)], { answer: 'count' }).n,
    lowest: () => query([{ step: 'orderBy', field: 'n' }, { step: 'limit', n: 10 }], { answer: 'rows', fields: ['n'] }).rows.map((row) => row.n),
    lowestVisited: () => query([{ step: 'orderBy', field: 'n' }, { step: 'limit', n: 10 }], { answer: 'ids' }, { explain: true }).visited,
    // A plan has no range step, so a caller reads the field of every note and keeps what is in the range
    atLeast: (least) => query([], { answer: 'rows', fields: ['n'] }).rows.filter((row) => row.n >= least).length,
    pageAfter: (cursor) => query([{ step: 'after', id: cursor }, { step: 'limit', n: 50 }], { answer: 'ids' }).ids,
    check: () => {
      const report = db.check();
      assert.deepEqual([report.differences.length, report.violations.length], [0, 0], 'elmdb: check() found a difference');
    },
    rebuild: () => db.rebuild(),
    bytes: () => statSync(join(dir, 'data.mdb')).size,
    compacted: () => {
      db.backup(join(root, 'elmdb-compacted'));
      return statSync(join(root, 'elmdb-compacted', 'data.mdb')).size;
    },
    close: () => db.close(),
  };
}

// SQLite with the same fields, indexes and a text index over the title, kept by triggers
function sqlite() {
  const file = join(root, 'sqlite.db');
  const db = new DatabaseSync(file);
  db.exec('PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF;');
  db.exec(`
    CREATE TABLE notes(id TEXT PRIMARY KEY, title TEXT NOT NULL, n REAL NOT NULL,
      status TEXT NOT NULL CHECK (status IN ('open','done')));
    CREATE INDEX notes_status ON notes(status);
    CREATE INDEX notes_n ON notes(n);`);
  // Some Node releases build SQLite without FTS5 (23.11 does): there SQLite keeps no text index and answers no search
  let text = true;
  try {
    db.exec(`
      CREATE VIRTUAL TABLE notes_fts USING fts5(title, content='notes', content_rowid='rowid');
      CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN
        INSERT INTO notes_fts(rowid, title) VALUES (new.rowid, new.title);
      END;`);
  } catch (error) {
    // Only a SQLite without the module: anything else wrong with the statements is a failure of this script
    if (!/fts5/i.test(String(error?.message))) throw error;
    text = false;
  }
  const insert = db.prepare('INSERT INTO notes(id, title, n, status) VALUES (?, ?, ?, ?)');
  const create = (one) => insert.run(one.id, one.title, one.n, one.status);
  const best = text && db.prepare('SELECT n.id FROM notes_fts f JOIN notes n ON n.rowid = f.rowid WHERE notes_fts MATCH ? ORDER BY f.rank LIMIT 10');
  const count = text && db.prepare('SELECT count(*) AS n FROM notes_fts WHERE notes_fts MATCH ?');
  const lowest = db.prepare('SELECT n FROM notes ORDER BY n LIMIT 10');
  const atLeast = db.prepare('SELECT count(*) AS n FROM notes WHERE n >= ?');
  const pageAfter = db.prepare('SELECT id FROM notes WHERE rowid > (SELECT rowid FROM notes WHERE id = ?) ORDER BY rowid LIMIT 50');
  return {
    version: db.prepare('SELECT sqlite_version() AS v').get().v,
    text,
    load: (notes) => {
      db.exec('BEGIN');
      notes.forEach(create);
      db.exec('COMMIT');
    },
    create,
    settle: () => {},
    best: (text) => best.all(text).map((row) => row.id),
    count: (text) => count.get(text).n,
    lowest: () => lowest.all().map((row) => row.n),
    atLeast: (least) => atLeast.get(least).n,
    pageAfter: (cursor) => pageAfter.all(cursor).map((row) => row.id),
    check: () => assert.equal(db.prepare('PRAGMA integrity_check').get().integrity_check, 'ok', 'sqlite: integrity_check'),
    rebuild: () => db.exec(`${text ? "INSERT INTO notes_fts(notes_fts) VALUES ('rebuild');" : ''} REINDEX;`),
    bytes: () => {
      db.exec('PRAGMA wal_checkpoint(TRUNCATE)');
      return statSync(file).size;
    },
    compacted: () => {
      db.exec(`VACUUM INTO '${join(root, 'sqlite-compacted.db')}'`);
      return statSync(join(root, 'sqlite-compacted.db')).size;
    },
    close: () => db.close(),
  };
}

// One engine's readings. Both are handed the same notes in the same order, the commits' own included
function measure(name, engine) {
  const out = { commits: [] };
  let made = 0;
  let extra = 0;
  let scatter = 0;
  // Terms that exist already, each some other note's number, spread over the notes so each is in a page of its own
  const rare = (count) => Array.from({ length: count }, () => String((scatter++ * 66667 + 13) % made)).join(' ');
  // What a create's commit costs: a note of three terms every note holds, and a message of thirty, fifteen of them
  // widely held and fifteen rare. Wall time as a median, and the process's CPU over them all, which a busy machine
  // inflates less. A commit's note sorts before every loaded one by its number, each a number of its own
  const commits = () => {
    const shapes = {
      small: () => 'the alpha note',
      message: () => `the note number ${WORDS.join(' ')} ${rare(15)}`,
    };
    const reading = { notes: made };
    for (const [shape, title] of Object.entries(shapes)) {
      const samples = [];
      const cpu = process.cpuUsage();
      for (let i = 0; i < COMMITS; i++) {
        const one = { id: `Note-x${extra}`, title: title(), n: -1 - extra, status: 'open' };
        extra++;
        const from = process.hrtime.bigint();
        engine.create(one);
        samples.push(ms(from));
      }
      const used = process.cpuUsage(cpu);
      reading[`${shape}Ms`] = round(median(samples));
      reading[`${shape}CpuMicros`] = round((used.user + used.system) / COMMITS, 1);
    }
    out.commits.push(reading);
  };
  const loadTo = (count) => {
    let took = 0;
    while (made < count) {
      const notes = Array.from({ length: Math.min(UNIT, count - made) }, (_, i) => note(made + i));
      const from = process.hrtime.bigint();
      engine.load(notes);
      took += ms(from);
      made += notes.length;
    }
    return took;
  };
  let load = loadTo(Math.floor(NOTES / 20));
  commits();
  load += loadTo(NOTES);
  engine.load(RANKED);
  extra += RANKED.length;
  engine.settle();
  out.loadSeconds = round(load / 1000, 2);
  progress(`${name}: ${NOTES} notes loaded in ${out.loadSeconds} s`);
  out.bytesPerNote = Math.round(engine.bytes() / (NOTES + extra));

  const answers = { lowest: engine.lowest(), atLeast: engine.atLeast(NOTES - 500), page: engine.pageAfter(id(NOTES - 200)) };
  // What every note holds, what a twelfth hold, and a word one letter off one a sixth hold
  if (engine.text) {
    Object.assign(answers, { everyCount: engine.count('the'), twelfthCount: engine.count('alpha'), twelfthBest: engine.best('alpha').length, ranked: engine.best('quokka') });
    out.searches = {
      everyBestTenMs: timed(5, () => engine.best('the')).ms,
      everyCountMs: timed(5, () => engine.count('the')).ms,
      twelfthBestTenMs: timed(5, () => engine.best('alpha')).ms,
      twelfthCountMs: timed(5, () => engine.count('alpha')).ms,
      misspelledMs: timed(5, () => engine.best('foxtrott', true)).ms,
      misspelledFound: engine.best('foxtrott', true).length,
    };
  }
  out.reads = {
    lowestTenMs: timed(5, () => engine.lowest()).ms,
    last500Ms: timed(3, () => engine.atLeast(NOTES - 500)).ms,
    pageNearEndMs: timed(5, () => engine.pageAfter(id(NOTES - 200))).ms,
  };
  if (engine.visited) {
    out.searches.everyBestTenVisited = engine.visited('the');
    out.reads.lowestTenVisited = engine.lowestVisited();
  }
  progress(`${name}: read`);
  commits();
  out.checkSeconds = seconds(engine.check);
  out.rebuildSeconds = seconds(() => {
    engine.rebuild();
    engine.settle();
  });
  out.bytesPerNoteCompacted = Math.round(engine.compacted() / (NOTES + extra));
  engine.close();
  progress(`${name}: checked, rebuilt and compacted`);
  return { out, answers };
}

const git = (...words) => execFileSync('git', words, { cwd: fileURLToPath(new URL('../../../', import.meta.url)), encoding: 'utf8' }).trim();
const report = {
  benchmark: 'against-sqlite',
  fixtureVersion: 2,
  notes: NOTES,
  source: { commit: git('rev-parse', 'HEAD'), dirty: Boolean(git('status', '--porcelain', '--untracked-files=normal')) },
  host: {
    cpu: cpus()[0].model, cores: cpus().length, memoryBytes: totalmem(), platform: `${platform()}-${arch()}`,
    node: process.version, loadBefore: round(loadavg()[0], 1),
  },
};
try {
  const lite = sqlite();
  report.host.sqlite = lite.version;
  report.host.sqliteText = lite.text;
  assert.ok(lite.text || !requireText, 'this Node\'s SQLite has no FTS5, and --require-text was given');
  const ours = measure('elmdb', elmdb());
  const theirs = measure('sqlite', lite);
  // The misspelled word is left out: SQLite finds nothing for it, which is its answer and not a difference in the notes
  for (const [read, answer] of Object.entries(theirs.answers)) {
    assert.deepEqual(ours.answers[read], answer, `elmdb and SQLite answered ${read} differently`);
  }
  // elmdb's own searches, held to what the notes say whether or not SQLite answered any
  assert.deepEqual(ours.answers.ranked, RANKED_BEST, 'elmdb ranked the notes holding a word most often out of order');
  assert.equal(ours.out.searches.misspelledFound, 10, 'elmdb found fewer than ten notes for a word one letter off');
  report.elmdb = ours.out;
  report.sqlite = theirs.out;
  report.host.loadAfter = round(loadavg()[0], 1);
} finally {
  rmSync(root, { recursive: true, force: true });
}

if (json) {
  console.log(JSON.stringify(report, null, 2));
} else {
  // A reading a line, `name=value`, which `cargo xtask compare` reads
  const lines = (prefix, value) =>
    typeof value === 'object'
      ? Object.entries(value).flatMap(([key, inner]) => lines(Array.isArray(value) ? `${prefix}_at${inner.notes}` : `${prefix}_${key}`, inner))
      : [`${prefix}=${value}`];
  console.log(`notes=${NOTES} commit=${report.source.commit.slice(0, 7)}${report.source.dirty ? '+changes' : ''} sqlite=${report.host.sqlite} load=${report.host.loadBefore}`);
  for (const engine of ['elmdb', 'sqlite']) console.log(lines(engine, report[engine]).join('\n'));
}
