// Synthetic recovery workloads only. Run each operation in a fresh process; resourceUsage includes native RSS.
// node scripts/measure-recovery.mjs seed /private/tmp/elmdb-recovery-run 10000 1024
// node scripts/measure-recovery.mjs check|rebuild|replace|replace-populated|backup /private/tmp/elmdb-recovery-run
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import * as fs from 'node:fs';
import { join, resolve } from 'node:path';
import { Packr } from 'msgpackr';

const { Database } = createRequire(import.meta.url)('../crates/elmdb-node/index.js');
const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });
const [mode, directory, rowsArg = '10000', bytesArg = '1024', profile = 'text'] = process.argv.slice(2);
const modes = ['seed', 'check', 'rebuild', 'replace', 'replace-populated', 'backup'];
if (!modes.includes(mode) || !directory) throw new Error('Usage: measure-recovery.mjs seed|check|rebuild|replace|replace-populated|backup <owned-workload-dir> [rows] [body-bytes] [text|vector]');
const root = resolve(directory);
const source = join(root, 'source');
const manifestPath = join(root, 'workload.json');
const owner = 'elmdb-recovery-measure-v1';
if (!['text', 'vector'].includes(profile)) throw new Error('Profile must be text or vector');
const schema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  relations: { refs: { cardinality: 'many-to-many' } },
  types: { Note: { fields: {
    title: { type: 'string' }, body: { type: 'string' }, slug: { type: 'string' }, state: { type: 'string' },
  }, indexes: {
    slug: { kind: 'value', fields: ['slug'], unique: true },
    state: { kind: 'value', fields: ['state'] },
    text: { kind: 'text', fields: { title: {}, body: {} } },
  } } },
};
if (profile === 'vector') {
  schema.types.Note.indexes.meaning = { kind: 'vector', from: ['body'], dims: 1536, model: 'synthetic-recovery' };
}
function diskBytes(path) {
  return fs.readdirSync(path, { withFileTypes: true }).reduce((sum, entry) => {
    const child = join(path, entry.name);
    return sum + (entry.isDirectory() ? diskBytes(child) : fs.statSync(child).size);
  }, 0);
}
let workload;
if (mode === 'seed') {
  const rows = Number(rowsArg), bodyBytes = Number(bytesArg);
  if (!Number.isSafeInteger(rows) || rows < 1 || rows > 1000000 || !Number.isSafeInteger(bodyBytes) || bodyBytes < 1 || bodyBytes > 262144) {
    throw new Error('Rows must be 1..1000000 and body bytes 1..262144');
  }
  fs.mkdirSync(root); // Refuse an existing directory rather than adopting somebody else's data.
  workload = { owner, profile, vectorDims: profile === 'vector' ? 1536 : 0, rows, bodyBytes, relations: Math.max(0, rows - 1) + Math.max(0, rows - 2), schema };
  fs.writeFileSync(manifestPath, JSON.stringify(workload, null, 2));
} else {
  workload = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  if (workload.owner !== owner || !fs.existsSync(join(source, 'data.mdb'))) throw new Error('Not an owned synthetic workload');
}
if (mode !== 'seed' && rowsArg === '--observe') {
  const scratch = fs.mkdtempSync(join(root, 'scratch-'));
  const child = spawn(process.execPath, [fileURLToPath(import.meta.url), mode, root], {
    env: { ...process.env, TMPDIR: scratch, TEMP: scratch, TMP: scratch }, stdio: ['ignore', 'pipe', 'inherit'],
  });
  let output = '', peakScratchBytes = 0, peakLogicalDiskBytes = diskBytes(root);
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', bytes => { output += bytes; });
  const sample = () => {
    // Scratch runs are renamed/deleted during merge; a vanished entry is a concurrent observation, not a failure.
    try { peakScratchBytes = Math.max(peakScratchBytes, diskBytes(scratch)); } catch (error) { if (error.code !== 'ENOENT') throw error; }
    try { peakLogicalDiskBytes = Math.max(peakLogicalDiskBytes, diskBytes(root)); } catch (error) { if (error.code !== 'ENOENT') throw error; }
  };
  const timer = setInterval(sample, 100);
  const code = await new Promise((resolve, reject) => { child.on('error', reject); child.on('close', resolve); });
  clearInterval(timer); sample();
  if (code !== 0) { console.error(`Measurement failed; owned scratch retained at ${scratch}`); process.exit(code ?? 1); }
  const report = JSON.parse(output);
  if (fs.readdirSync(scratch).length) throw new Error(`Scratch cleanup incomplete: ${scratch}`);
  fs.rmdirSync(scratch);
  console.log(JSON.stringify({ ...report, peakScratchBytes, peakLogicalDiskBytes,
    diskSampling: '100 ms metadata observer; sampled peaks are lower bounds; filesystem allocation may differ' }));
  process.exit(0);
}
const beforeBytes = diskBytes(root);
const started = performance.now();
let target;
if (mode === 'seed') {
  const db = Database.open(source, { schema, syncs: false });
  try {
    for (let first = 0; first < workload.rows; first += 500) {
      const ops = [];
      for (let i = first; i < Math.min(first + 500, workload.rows); ++i) {
        const phrase = `Conversation memory ${i} project agent recovery document message context. `;
        const body = phrase.repeat(Math.ceil(workload.bodyBytes / phrase.length)).slice(0, workload.bodyBytes);
        ops.push({ op: 'create', type: 'Note', id: `Note-${i}`, fields: {
          title: `Message ${i}`, body, slug: `message-${i}`, state: ['open', 'done', 'archived'][i % 3],
        } });
        for (const previous of [i - 1, i - 2].filter(n => n >= 0)) {
          ops.push({ op: 'link', source: `Note-${i}`, target: `Note-${previous}`, kind: 'refs' });
        }
      }
      db.write(packr.pack({ ops, now: 1000 }));
    }
    if (workload.vectorDims) {
      let embedded = 0;
      for (;;) {
        const [pending] = packr.unpack(db.readSync(packr.pack({
          reads: [{ read: 'pendingEmbeddings', type: 'Note', index: 'meaning', limit: 100 }],
        })));
        if (!pending.chunks.length) break;
        const ops = pending.chunks.map((chunk) => {
          const ordinal = ++embedded;
          return { op: 'embed', id: chunk.id, index: 'meaning', chunk: chunk.chunk, hash: chunk.hash,
            model: 'synthetic-recovery', vector: Array.from({ length: workload.vectorDims }, (_, j) =>
              Math.sin(ordinal * (j + 1) * 0.001)) };
        });
        const result = packr.unpack(db.write(packr.pack({ ops, now: 1000 })));
        if (result.results.some(row => row.kind !== 'done')) throw new Error('Synthetic embedding refused');
      }
      workload.embeddings = embedded;
      fs.writeFileSync(manifestPath, JSON.stringify(workload, null, 2));
    }
  } finally { db.close(); }
} else if (mode === 'replace' || mode === 'replace-populated') {
  target = fs.mkdtempSync(join(root, 'replace-'));
  if (mode === 'replace-populated') {
    const incumbent = Database.open(source, { readOnly: true });
    try { incumbent.backup(target); } finally { incumbent.close(); }
  }
  const destination = Database.open(target, { schema: workload.schema });
  try { destination.replaceWith(source); } finally { destination.close(); }
} else {
  const db = Database.open(source, { readOnly: mode === 'check' || mode === 'backup' });
  try {
    if (mode === 'check') {
      const report = db.check();
      if (report.differences.length || report.violations.length) throw new Error(JSON.stringify(report));
    } else if (mode === 'rebuild') {
      db.rebuild();
    } else if (mode === 'backup') {
      target = fs.mkdtempSync(join(root, 'backup-'));
      db.backup(target);

    }
  } finally { db.close(); }
}
const elapsedMs = performance.now() - started;
console.log(JSON.stringify({
  mode, root, target, profile: workload.profile ?? 'text', vectorDims: workload.vectorDims ?? 0, embeddings: workload.embeddings ?? 0, rows: workload.rows, bodyBytes: workload.bodyBytes, relations: workload.relations,
  elapsedMs, maxRssKiB: process.resourceUsage().maxRSS,
  sourceFileBytes: fs.statSync(join(source, 'data.mdb')).size,
  logicalDiskBefore: beforeBytes, logicalDiskAfter: diskBytes(root),
  writerOccupancyUpperBoundMs: ['replace', 'replace-populated', 'rebuild'].includes(mode) ? elapsedMs : mode === 'seed' ? null : 0,
  destinationInitiallyPopulated: mode === 'replace-populated',
  conditions: 'replace-populated includes incumbent backup preparation; fresh Node process; filesystem cache uncontrolled; seed syncs disabled, maintenance defaults; synthetic data',
}));
