/**
 * The package as npm publishes it, in a consumer that installs it with `elmdb` beside it and has no Node types: what it
 * ships, that every export resolves to declarations a consumer compiles against, on the oldest TypeScript the package
 * supports and the current one, that each export loads in a plain Node process, and that no export's type holds `any`.
 */
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { pathToFileURL } from 'node:url';
import ts from 'typescript';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import {
  MANIFEST, MODULE_RESOLUTIONS, codeExports, compilers, consumerOptions, installPacked, run, type Consumer,
} from './_support/packed.ts';

let consumer: Consumer;
beforeAll(async () => {
  consumer = await installPacked();
});
afterAll(() => consumer?.remove());

const installedManifest = () =>
  JSON.parse(fs.readFileSync(path.join(consumer.installed, 'package.json'), 'utf8')) as { exports: Record<string, unknown> };

describe('the packed package', () => {
  it('ships its build, its manifest and its license, and no source maps', () => {
    expect(fs.readdirSync(consumer.installed).sort()).toEqual(['LICENSE', 'dist', 'package.json']);
    const shipped = fs.readdirSync(path.join(consumer.installed, 'dist'), { recursive: true }).map(String);
    expect(shipped.filter((file) => file.endsWith('.js')).length, 'no built module shipped').toBeGreaterThan(0);
    // A map would point at `src`, which isn't published
    expect(shipped.filter((file) => file.endsWith('.map'))).toEqual([]);
  });

  it('exports declarations and code for every entry but its manifest, and no source condition', () => {
    const entries = Object.entries(installedManifest().exports);
    expect(entries.filter(([, target]) => typeof target !== 'object' || target === null).map(([key]) => key)).toEqual(['./package.json']);
    // An export a consumer resolves to `src`, which isn't shipped, would fail them under that condition
    expect(entries.filter(([, target]) => typeof target === 'object' && target !== null)
      .filter(([, target]) => Object.keys(target as object).sort().join() !== 'default,types')
      .map(([key]) => key)).toEqual([]);
    expect(codeExports(consumer)).toContain(MANIFEST.name);
  });

  const matrix = (['floor', 'current'] as const).flatMap((tsc) => MODULE_RESOLUTIONS.map((moduleResolution) => ({ tsc, moduleResolution })));

  it.each(matrix)('compiles for a consumer with no Node types under TypeScript $tsc, moduleResolution $moduleResolution', async ({ tsc, moduleResolution }) => {
    const dir = path.join(consumer.dir, `compile-${tsc}-${moduleResolution}`);
    fs.mkdirSync(dir);
    fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify({ name: 'consumer-compile', private: true, type: 'module' }));
    fs.writeFileSync(path.join(dir, 'tsconfig.json'), JSON.stringify({ compilerOptions: consumerOptions(moduleResolution), include: ['*.ts'] }));
    // Every export, so one that resolves to nothing, or to code without declarations (`any` under noImplicitAny), fails
    fs.writeFileSync(path.join(dir, 'exports.ts'), codeExports(consumer).map((specifier, i) => `export * as m${i} from '${specifier}';`).join('\n'));
    // Typed reads over a schema a consumer declares, and an engine over a database it holds
    fs.writeFileSync(path.join(dir, 'typed.ts'), [
      `import { createEarsEngine, defineEars, type EARS, type EarsDatabase, type UntypedSchema } from '${MANIFEST.name}';`,
      'type IsAny<T> = 0 extends 1 & T ? true : false;',
      "interface NoteEntity { entityType: 'Note'; title: string }",
      'declare const database: EarsDatabase;',
      'const engine = createEarsEngine({ database });',
      "const { query } = defineEars<UntypedSchema & { shapes: { Note: NoteEntity }; names: 'Note' }>(() => engine);",
      "declare const noteId: EARS.EntityId<'Note'>;",
      'const note = query.ids(noteId).pickOne()!;',
      'export const title: string = note.title;',
      'export const titleNotAny: IsAny<typeof note.title> = false;',
      "// @ts-expect-error a declared shape's field is typed",
      'export const titleNumber: number = note.title;',
      "engine.create('Note', { title: 'held' });",
      'engine.admin.clear();',
    ].join('\n'));
    const { code, output } = await run(process.execPath, [compilers()[tsc], '-p', dir], dir);
    expect(output).toBe('');
    expect(code).toBe(0);
  });

  it('loads each export in a plain Node process, with elmdb beside it', async () => {
    // A plain Node process resolves as a consumer's runtime does: no condition but Node's own
    const specifiers = codeExports(consumer);
    const { code, output } = await run(process.execPath, ['--input-type=module', '-e', [
      `const specifiers = ${JSON.stringify(specifiers)};`,
      'const loaded = [];',
      'for (const specifier of specifiers) loaded.push([import.meta.resolve(specifier), Object.keys(await import(specifier)).length]);',
      'process.stdout.write(JSON.stringify(loaded));',
    ].join('\n')], consumer.dir);
    expect(code, output).toBe(0);
    const loaded = JSON.parse(output) as Array<[string, number]>;
    const dist = pathToFileURL(fs.realpathSync(path.join(consumer.installed, 'dist'))).href;
    expect(loaded.filter(([url, exported]) => !url.startsWith(`${dist}/`) || exported === 0)).toEqual([]);
    expect(loaded).toHaveLength(specifiers.length);
  });

  it('exposes no any in any export', () => {
    expect(exportsWithAny(consumer.installed, [fs.realpathSync(consumer.installed)])).toEqual([]);
  });
});

/**
 * `<subpath> <export>` for each export of the package at `packageDir` whose type contains `any`, walking into the
 * declarations under `firstPartyRoots` wherever they're reached. A dependency's types (elmdb's, msgpackr's) are its
 * own business and aren't walked; the standard library's are, through the type arguments given them.
 *
 * What counts as reaching `any`: an export's signature, its `this` parameter, public members, type arguments, namespace
 * members, conditional branches, and type parameter constraints or defaults, a few levels deep.
 */
function exportsWithAny(packageDir: string, firstPartyRoots: string[]): string[] {
  const manifest = JSON.parse(fs.readFileSync(path.join(packageDir, 'package.json'), 'utf8')) as { exports: Record<string, unknown> };
  const entries = Object.entries(manifest.exports)
    .filter(([, target]) => typeof target === 'object' && target !== null && 'types' in target)
    .map(([key, target]) => [key, path.join(packageDir, (target as { types: string }).types)] as const);
  const program = ts.createProgram(entries.map(([, file]) => file), {
    strict: true, skipLibCheck: true, noEmit: true, types: [], target: ts.ScriptTarget.ES2022, lib: ['lib.es2022.d.ts'],
    module: ts.ModuleKind.ESNext, moduleResolution: ts.ModuleResolutionKind.Bundler,
  });
  const checker = program.getTypeChecker();
  const standardLibrary = (node: ts.Node) => program.isSourceFileDefaultLibrary(node.getSourceFile());
  const firstParty = (file: string) => firstPartyRoots.some((root) => fs.realpathSync(file).startsWith(root + path.sep));
  const thirdParty = (node: ts.Node) => {
    const file = node.getSourceFile().fileName;
    return /\/node_modules\//.test(file) && !standardLibrary(node) && !firstParty(file);
  };
  const typeArguments = (type: ts.Type) => (type.flags & ts.TypeFlags.Object && (type as ts.TypeReference).target
    ? checker.getTypeArguments(type as ts.TypeReference)
    : []);

  /** `<T = any>` (a caller who leaves T out gets any) or `<T extends …any…>` (any is what T accepts and reads as) */
  function typeParameterLeaks(parameter: ts.TypeParameter, seen: Set<ts.Type>, depth: number): boolean {
    const fallback = checker.getDefaultFromTypeParameter(parameter);
    if (fallback && containsAny(fallback, seen, depth + 1)) return true;
    const constraint = checker.getBaseConstraintOfType(parameter);
    return !!constraint && containsAny(constraint, seen, depth + 1);
  }

  function containsAny(type: ts.Type, seen: Set<ts.Type>, depth: number): boolean {
    if (type.flags & ts.TypeFlags.Any) return true;
    if (seen.has(type) || depth > 6) return false;
    seen.add(type);
    const alias = type.aliasSymbol?.declarations?.[0];
    const declaration = type.symbol?.declarations?.[0];
    if ((alias && thirdParty(alias)) || (declaration && thirdParty(declaration))) return false;
    if ((alias && standardLibrary(alias)) || (declaration && standardLibrary(declaration))) {
      const args = [...(alias && standardLibrary(alias) ? type.aliasTypeArguments ?? [] : []), ...typeArguments(type)];
      return args.some((t) => containsAny(t, seen, depth + 1));
    }
    if (type.isUnionOrIntersection()) return type.types.some((t) => containsAny(t, seen, depth + 1));
    if (type.flags & ts.TypeFlags.TypeParameter) return typeParameterLeaks(type as ts.TypeParameter, seen, depth);
    if (type.flags & ts.TypeFlags.Conditional) {
      // A generic conditional keeps its branches unresolved until it's instantiated, so walk the written ones
      const conditional = type as ts.ConditionalType;
      const branches = [conditional.root.node.trueType, conditional.root.node.falseType].map((node) => checker.getTypeFromTypeNode(node));
      if ([conditional.checkType, conditional.extendsType, ...branches].some((t) => containsAny(t, seen, depth + 1))) return true;
    }
    if (type.flags & ts.TypeFlags.IndexedAccess) {
      const indexed = type as ts.IndexedAccessType;
      if ([indexed.objectType, indexed.indexType].some((t) => containsAny(t, seen, depth + 1))) return true;
    }
    for (const signature of [...type.getCallSignatures(), ...type.getConstructSignatures()]) {
      if (containsAny(signature.getReturnType(), seen, depth + 1)) return true;
      if (signature.getTypeParameters()?.some((p) => typeParameterLeaks(p, seen, depth))) return true;
      // `this: any` is a parameter a caller's receiver is checked against, and it isn't in getParameters()
      const receiver = signature.thisParameter;
      if (receiver && containsAny(checker.getTypeOfSymbol(receiver), seen, depth + 1)) return true;
      if (signature.getParameters().some((p) => containsAny(checker.getTypeOfSymbol(p), seen, depth + 1))) return true;
    }
    if (!(type.flags & ts.TypeFlags.Object)) return false;
    if (typeArguments(type).some((t) => containsAny(t, seen, depth + 1))) return true;
    if (checker.getIndexInfosOfType(type).some((info) => containsAny(info.type, seen, depth + 1))) return true;
    return type.getProperties().some((property) => {
      const member = property.valueDeclaration ?? property.declarations?.[0];
      if (member && (thirdParty(member) || ts.getCombinedModifierFlags(member as ts.Declaration) & (ts.ModifierFlags.Private | ts.ModifierFlags.Protected))) return false;
      return containsAny(checker.getTypeOfSymbol(property), seen, depth + 1);
    });
  }

  function symbolLeaks(symbol: ts.Symbol, label: string, found: string[], depth: number): void {
    const target = symbol.flags & ts.SymbolFlags.Alias ? checker.getAliasedSymbol(symbol) : symbol;
    const declaration = target.declarations?.[0];
    if (!declaration || thirdParty(declaration)) return;
    if (target.flags & ts.SymbolFlags.Module && depth < 4) {
      // A namespace's type-only members (`namespace EARS { type X = any }`) are in neither its value type nor its
      // declared type, so each export is checked on its own
      for (const member of checker.getExportsOfModule(target)) symbolLeaks(member, `${label}.${member.name}`, found, depth + 1);
      // A namespace with no value side has no type of its own, and the checker answers `any` for it
      if (!(target.flags & ts.SymbolFlags.ValueModule)) return;
    }
    const type = target.flags & ts.SymbolFlags.Type && !(target.flags & ts.SymbolFlags.Value)
      ? checker.getDeclaredTypeOfSymbol(target)
      : checker.getTypeOfSymbolAtLocation(target, declaration);
    const typeParameters = ts.getEffectiveTypeParameterDeclarations(declaration as ts.DeclarationWithTypeParameters)
      .map((parameter) => checker.getTypeAtLocation(parameter) as ts.TypeParameter);
    if (containsAny(type, new Set(), 0) || typeParameters.some((p) => typeParameterLeaks(p, new Set(), 0))) found.push(label);
  }

  const found: string[] = [];
  for (const [key, file] of entries) {
    const module = checker.getSymbolAtLocation(program.getSourceFile(file)!);
    for (const symbol of module ? checker.getExportsOfModule(module) : []) symbolLeaks(symbol, `${key} ${symbol.name}`, found, 0);
  }
  return [...new Set(found)];
}

/**
 * The detector, proved against declarations that leak on purpose: without these the case above asserts only that
 * today's package is clean, and a walk that stopped following `this` parameters, conditional branches or namespace
 * members would keep passing.
 */
describe('the any detector', () => {
  const fixtures: string[] = [];
  afterAll(() => {
    for (const dir of fixtures.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
  });

  /** A package whose one entry declares `source`, scanned as first-party */
  function scan(source: string): string[] {
    const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'ears-any-detector-')));
    fixtures.push(dir);
    fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify({
      name: '@fixture/leaky', version: '0.0.0', exports: { '.': { types: './index.d.ts', default: './index.js' } },
    }));
    fs.writeFileSync(path.join(dir, 'index.d.ts'), source);
    return exportsWithAny(dir, [dir]);
  }

  it.each([
    ['a type-parameter default', 'export declare function f<T = any>(x: T): void;'],
    ['a type-parameter constraint', 'export declare function f<T extends { leak: any }>(x: T): void;'],
    ['a this parameter', 'export declare function f(this: any, x: number): void;'],
    ['a return type', 'export declare function f(): any;'],
    ['a generic conditional branch', 'export type f<T> = T extends string ? any : number;'],
    ['a public member', 'export interface f { leak: any }'],
    ['a type argument', 'export declare const f: Array<any>;'],
    ['an index signature', 'export interface f { [key: string]: any }'],
  ])('catches %s', (_form, source) => {
    expect(scan(source)).toEqual(['. f']);
  });

  it('reports nothing for declarations that expose no any', () => {
    expect(scan([
      'export declare function fn<T extends { ok: string } = { ok: string }>(this: void, x: T): T;',
      'export interface Shape { value: unknown; nested: { list: readonly string[] } }',
      'export declare namespace NS { type Alias = string }',
      'export type Cond<T> = T extends string ? number : boolean;',
    ].join('\n'))).toEqual([]);
  });

  it('catches a type-only namespace member, naming the member', () => {
    expect(scan('export declare namespace f { type Leak = any; }')).toEqual(['. f.Leak']);
  });

  it("leaves a dependency's type alone, its any being its own business", () => {
    // The dependency sits beside the package, not inside it: first-party is decided by path prefix
    const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'ears-any-detector-third-')));
    fixtures.push(root);
    const other = path.join(root, 'node_modules', 'other');
    fs.mkdirSync(other, { recursive: true });
    fs.writeFileSync(path.join(other, 'package.json'), JSON.stringify({ name: 'other', version: '0.0.0', types: './index.d.ts' }));
    fs.writeFileSync(path.join(other, 'index.d.ts'), 'export interface Theirs { leak: any }\n');
    const pkg = path.join(root, 'pkg');
    fs.mkdirSync(pkg, { recursive: true });
    fs.writeFileSync(path.join(pkg, 'package.json'), JSON.stringify({
      name: '@fixture/leaky', version: '0.0.0', exports: { '.': { types: './index.d.ts', default: './index.js' } },
    }));
    fs.writeFileSync(path.join(pkg, 'index.d.ts'), "import type { Theirs } from 'other';\nexport declare const f: Theirs;\n");
    expect(exportsWithAny(pkg, [pkg])).toEqual([]);
  });
});
