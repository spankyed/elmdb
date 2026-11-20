// The API reports, one per export with declarations (`etc/index.api.md` for `.`, `etc/database.api.md` for
// `./database`), from declarations emitted into `.temp/api-types`. Fails when a report is out of date, when API
// Extractor reports a problem, or when a report names no export; `--local` writes them instead.
//
//   node packages/ears/scripts/api-report.mjs [--local]     (npm run api:check / api:update -w @elmdb/ears)
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import { createRequire } from 'node:module';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { CompilerState, Extractor, ExtractorConfig, ExtractorLogLevel } from '@microsoft/api-extractor';

const PACKAGE_DIR = fileURLToPath(new URL('..', import.meta.url));
const local = process.argv.includes('--local');
const manifest = JSON.parse(fs.readFileSync(path.join(PACKAGE_DIR, 'package.json'), 'utf8'));
const typesDir = path.join(PACKAGE_DIR, '.temp', 'api-types');
const reportFolder = path.join(PACKAGE_DIR, 'etc');
const tsconfig = path.join(PACKAGE_DIR, 'tsconfig.api-extractor.json');

// Declarations of the source as it is, not of `dist`, which may be another build's
fs.rmSync(typesDir, { recursive: true, force: true });
execFileSync(process.execPath, [createRequire(import.meta.url).resolve('typescript/bin/tsc'), '-p', tsconfig], { stdio: 'inherit' });

/** `.` is `index.api.md`, `./a/b` is `a.b.api.md` */
const reportName = (key) => `${key === '.' ? 'index' : key.slice(2).replaceAll('/', '.')}.api.md`;

/** Each export that declares `types`, with the emitted declaration it names: `./dist/x.d.ts` is `.temp/api-types/x.d.ts` */
const entries = Object.entries(manifest.exports)
  .filter(([, target]) => typeof target === 'object' && target !== null && 'types' in target)
  .map(([key, target]) => [key, path.join(typesDir, target.types.replace(/^\.\/dist\//, ''))]);

const configFor = (key, declaration) => ExtractorConfig.prepare({
  configObject: {
    projectFolder: PACKAGE_DIR,
    mainEntryPointFilePath: declaration,
    // The committed reports end lines with LF; API Extractor's default is CRLF
    newlineKind: 'lf',
    compiler: { tsconfigFilePath: tsconfig },
    apiReport: { enabled: true, reportFolder, reportFileName: reportName(key), reportTempFolder: path.join(PACKAGE_DIR, '.temp') },
    docModel: { enabled: false },
    dtsRollup: { enabled: false },
    tsdocMetadata: { enabled: false },
    // prepare() applies none of API Extractor's defaults, whose fallback reports nothing at all: these mirror them,
    // but for the two turned off below
    messages: {
      compilerMessageReporting: { default: { logLevel: ExtractorLogLevel.Warning } },
      extractorMessageReporting: {
        default: { logLevel: ExtractorLogLevel.Warning },
        // A type an entry names without exporting it: most are exported by the other entry, where a consumer names them
        'ae-forgotten-export': { logLevel: ExtractorLogLevel.None },
        'ae-incompatible-release-tags': { logLevel: ExtractorLogLevel.Warning, addToApiReportFile: true },
        'ae-internal-missing-underscore': { logLevel: ExtractorLogLevel.Warning, addToApiReportFile: true },
        'ae-internal-mixed-release-tag': { logLevel: ExtractorLogLevel.Warning, addToApiReportFile: true },
        'ae-undocumented': { logLevel: ExtractorLogLevel.None },
        'ae-unresolved-inheritdoc-reference': { logLevel: ExtractorLogLevel.Warning, addToApiReportFile: true },
        'ae-unresolved-inheritdoc-base': { logLevel: ExtractorLogLevel.Warning, addToApiReportFile: true },
        'ae-wrong-input-file-type': { logLevel: ExtractorLogLevel.Error },
        // Every export is public API; a tag is only how `@internal` is marked
        'ae-missing-release-tag': { logLevel: ExtractorLogLevel.None },
      },
      // TSDoc's syntax isn't part of the contract the reports hold
      tsdocMessageReporting: { default: { logLevel: ExtractorLogLevel.None } },
    },
  },
  configObjectFullPath: undefined,
  packageJsonFullPath: path.join(PACKAGE_DIR, 'package.json'),
});

if (local) fs.mkdirSync(reportFolder, { recursive: true });
const prepared = entries.map(([key, declaration]) => ({ key, config: configFor(key, declaration) }));
// One program for every entry, as `compilerState` is for, rather than one built per entry
const compilerState = CompilerState.create(prepared[0].config, { additionalEntryPoints: entries.slice(1).map(([, declaration]) => declaration) });
let failed = 0;
let changed = 0;
for (const { key, config } of prepared) {
  const result = Extractor.invoke(config, { localBuild: local, showVerboseMessages: false, compilerState });
  if (result.apiReportChanged) changed++;
  if (!result.succeeded) {
    failed++;
    console.error(`${manifest.name}${key.slice(1)}: ${result.errorCount} error(s), ${result.warningCount} warning(s)${result.apiReportChanged ? ', report out of date' : ''}`);
  }
}
const expected = new Set(entries.map(([key]) => reportName(key)));
for (const file of fs.existsSync(reportFolder) ? fs.readdirSync(reportFolder).filter((f) => f.endsWith('.api.md') && !expected.has(f)) : []) {
  changed++;
  if (local) fs.rmSync(path.join(reportFolder, file));
  else { failed++; console.error(`etc/${file} names no export; npm run api:update -w @elmdb/ears removes it`); }
}
if (failed > 0) {
  console.error(`${manifest.name}: the API reports differ from the source; review the change and run npm run api:update -w @elmdb/ears`);
  process.exit(1);
}
console.log(local ? `${manifest.name}: ${changed} of ${expected.size} API reports updated` : `${manifest.name}: ${expected.size} API reports up to date`);
