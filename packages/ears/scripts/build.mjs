// Builds `dist/`: ESM and declarations from `tsconfig.package.json`, no source maps (`src` doesn't ship).
//
//   node packages/ears/scripts/build.mjs        (npm run build -w @elmdb/ears)
//
// The tree is emitted into a staging directory and checked there, and renamed over `dist` only once every check has
// passed, so a reader never finds `dist` half-built or built by a run that failed.
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import { builtinModules, createRequire } from 'node:module';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

/** This package's directory */
const PACKAGE_DIR = fileURLToPath(new URL('..', import.meta.url));
const require = createRequire(path.join(PACKAGE_DIR, 'package.json'));
const ts = require('typescript');

const srcDir = path.join(PACKAGE_DIR, 'src');
const distDir = path.join(PACKAGE_DIR, 'dist');
/** Inside the package, so the rename over `dist` can't cross a filesystem, and under `.temp/`, which git ignores */
const stagedDir = path.join(PACKAGE_DIR, '.temp', 'build');
const manifest = JSON.parse(fs.readFileSync(path.join(PACKAGE_DIR, 'package.json'), 'utf8'));

const walk = (dir) => fs.readdirSync(dir, { withFileTypes: true })
  .flatMap((entry) => entry.isDirectory() ? walk(path.join(dir, entry.name)) : [path.join(dir, entry.name)]);
const isDeclaration = (file) => file.endsWith('.d.ts');

/**
 * Rewrites relative `./x.ts` specifiers to `./x.js` in the emitted declarations. `rewriteRelativeImportExtensions`
 * rewrites them in the JavaScript only, and a declaration left naming `./x.ts`, a file `dist` doesn't hold, resolves
 * only through TypeScript's own substitution, which a bundler or a documentation tool needn't make. The specifiers
 * come from `ts.preProcessFile`, which reports every form an import takes with its position and knows what a comment
 * is, where a pattern over the text would miss some forms and rewrite an example in a doc comment.
 */
function rewriteDeclarationExtensions(dir) {
  for (const file of walk(dir).filter(isDeclaration)) {
    const before = fs.readFileSync(file, 'utf8');
    // A reference's `pos` is its opening quote. Rewritten from the last, so no edit moves a position still to come
    const relative = ts.preProcessFile(before, true, true).importedFiles
      .filter(({ fileName }) => /^\.\.?\//.test(fileName) && fileName.endsWith('.ts') && !fileName.endsWith('.d.ts'))
      .sort((a, b) => b.pos - a.pos);
    let after = before;
    for (const { fileName, pos } of relative) {
      const start = pos + 1;
      const stop = start + fileName.length;
      // A TypeScript whose positions meant something else would have every declaration rewritten blind
      if (before.slice(start, stop) !== fileName) {
        throw new Error(`${file}: expected "${fileName}" at ${start}, found "${before.slice(start, stop)}": ts.preProcessFile's positions have changed`);
      }
      after = `${after.slice(0, start)}${fileName.slice(0, -'.ts'.length)}.js${after.slice(stop)}`;
    }
    if (after !== before) fs.writeFileSync(file, after);
  }
}

/** The package a bare specifier names: `@scope/name` or `name`, without a subpath */
const packageOf = (specifier) => specifier.split('/').slice(0, specifier.startsWith('@') ? 2 : 1).join('/');

/**
 * Refuses a shipped module that imports a package the manifest declares neither as a dependency nor as a peer, the
 * declarations' imports included: a type-only import is erased from the JavaScript, so an undeclared one would ship
 * unseen, and fail only the consumer who compiles against it.
 */
function assertImportsDeclared(dir) {
  const declared = new Set([manifest.name, ...Object.keys(manifest.dependencies ?? {}), ...Object.keys(manifest.peerDependencies ?? {})]);
  const builtins = new Set([...builtinModules, ...builtinModules.map((name) => `node:${name}`)]);
  const undeclared = new Map();
  for (const file of walk(dir).filter((f) => f.endsWith('.js') || isDeclaration(f))) {
    const info = ts.preProcessFile(fs.readFileSync(file, 'utf8'), true, true);
    // In a declaration, `/// <reference types="x" />` pulls in x's declarations as surely as an import does; in
    // JavaScript it is a comment `tsc` copied from the source
    for (const { fileName } of [...info.importedFiles, ...(isDeclaration(file) ? info.typeReferenceDirectives : [])]) {
      if (/^[.#/]/.test(fileName) || builtins.has(fileName) || declared.has(packageOf(fileName))) continue;
      undeclared.set(packageOf(fileName), [...(undeclared.get(packageOf(fileName)) ?? []), path.relative(dir, file)]);
    }
  }
  if (undeclared.size > 0) {
    throw new Error(`Built modules import packages ${manifest.name}'s manifest doesn't declare:\n${
      [...undeclared].map(([name, files]) => `  ${name} <- ${files.slice(0, 3).join(', ')}`).join('\n')}`);
  }
}

/** Refuses a tree that lacks a file an export names: the targets under `dist`, read in the staged tree */
function assertExportTargetsBuilt(dir) {
  const targets = (entry) => typeof entry === 'string' ? [entry] : Object.values(entry).flatMap(targets);
  const missing = Object.values(manifest.exports).flatMap(targets)
    .filter((target) => target.startsWith('./dist/'))
    .filter((target) => !fs.existsSync(path.join(dir, target.slice('./dist/'.length))));
  if (missing.length > 0) throw new Error(`Export targets were not built:\n  ${missing.join('\n  ')}`);
}

fs.rmSync(stagedDir, { recursive: true, force: true });
fs.mkdirSync(stagedDir, { recursive: true });
// `--outDir` overrides the config's; `rootDir` is `src` either way, so the layout is the same
execFileSync(process.execPath, [require.resolve('typescript/bin/tsc'), '-p', path.join(PACKAGE_DIR, 'tsconfig.package.json'), '--outDir', stagedDir], { stdio: 'inherit' });
// Hand-written declarations ship as they are written
for (const file of walk(srcDir).filter(isDeclaration)) {
  fs.mkdirSync(path.dirname(path.join(stagedDir, path.relative(srcDir, file))), { recursive: true });
  fs.copyFileSync(file, path.join(stagedDir, path.relative(srcDir, file)));
}
rewriteDeclarationExtensions(stagedDir);
assertImportsDeclared(stagedDir);
assertExportTargetsBuilt(stagedDir);
// Every check has passed against the staged tree, so this is the moment it becomes the built one
const previous = path.join(PACKAGE_DIR, '.temp', 'dist-previous');
fs.rmSync(previous, { recursive: true, force: true });
if (fs.existsSync(distDir)) fs.renameSync(distDir, previous);
fs.renameSync(stagedDir, distDir);
fs.rmSync(previous, { recursive: true, force: true });
console.log(`Built ${manifest.name}@${manifest.version} into ${path.relative(process.cwd(), distDir)}`);
