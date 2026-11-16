// elmdb's diagnostics collected from Node (`docs/evidence/diagnostics.md`): a database written to and rebuilt, and the
// spans its operations reported. Needs a binding built with the `diagnostics` feature, which the default build is not:
//
//   cargo build -p elmdb-node --release --features diagnostics
//   cp target/release/libelmdb_node.dylib /tmp/elmdb-diagnostics.node   # .so on Linux, .dll on Windows
//   node crates/elmdb-node/examples/diagnostics.mjs /tmp/elmdb-diagnostics.node
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { Packr } from 'msgpackr';

const binding = process.argv[2];
if (!binding) throw new Error('usage: node diagnostics.mjs <a binding built with --features diagnostics>');
const { Database, collectDiagnostics, takeDiagnostics } = createRequire(import.meta.url)(binding);
if (!collectDiagnostics) throw new Error(`${binding} was built without the diagnostics feature`);

const packr = new Packr({ useRecords: false, moreTypes: true, mapsAsObjects: true });
const schema = {
  $schema: 'https://elmdb.dev/schema/1.json',
  types: { Note: { fields: { title: { type: 'string' } }, indexes: { title: { kind: 'value', fields: ['title'] } } } },
};

collectDiagnostics();
const dir = mkdtempSync(join(tmpdir(), 'elmdb-diagnostics-'));
const db = Database.open(dir, { schema, syncs: false });
try {
  const ops = Array.from({ length: 1000 }, (_, i) => ({ op: 'create', type: 'Note', fields: { title: `note ${i}` } }));
  db.write(packr.pack({ ops, now: 1000 }));
  db.rebuild('Note', 'title');
} finally {
  db.close();
  rmSync(dir, { recursive: true, force: true });
}
const { spans, dropped } = takeDiagnostics();
for (const span of spans) console.log(span);
console.log(`${spans.length} spans, ${dropped} dropped`);
