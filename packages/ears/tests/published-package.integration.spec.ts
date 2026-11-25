/**
 * The package as npm publishes it, in a consumer that installs it with `elmdb` beside it and has no Node types: what it
 * ships, that every export compiles for a consumer on the oldest TypeScript the package supports and the current one,
 * that each loads in a plain Node process, and that no export's type holds `any`.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import { pathToFileURL } from 'node:url';
import ts from 'typescript';
import { describe, expect, it } from 'vitest';
import {
  MANIFEST, MODULE_MANIFEST, MODULE_RESOLUTIONS, codeExports, compilers, consumerOptions, packedConsumer, readManifest, run,
  tempDirs, writeFiles,
} from './_support/packed.ts';

const consumer = packedConsumer();

/** Typed reads over a schema a consumer declares, and an engine over a database it holds */
const TYPED = `
import { createEarsEngine, defineEars, type EARS, type EarsDatabase, type UntypedSchema } from '${MANIFEST.name}';
interface NoteEntity { entityType: 'Note'; title: string }
declare const database: EarsDatabase;
const engine = createEarsEngine({ database });
const { query } = defineEars<UntypedSchema & { shapes: { Note: NoteEntity }; names: 'Note' }>(() => engine);
declare const noteId: EARS.EntityId<'Note'>;
const note = query.ids(noteId).pickOne()!;
export const title: string = note.title;
// @ts-expect-error a declared shape's field is typed: neither any nor unknown
export const titleNumber: number = note.title;
engine.create('Note', { title: 'held' });
engine.admin.clear();
`;

describe('the packed package', () => {
  it('ships its build, its manifest and its licenses, and no source maps', () => {
    expect(fs.readdirSync(consumer().installed).sort()).toEqual(['LICENSE-APACHE', 'LICENSE-MIT', 'dist', 'package.json']);
    const shipped = fs.readdirSync(path.join(consumer().installed, 'dist'), { recursive: true }).map(String);
    expect(shipped.filter((file) => file.endsWith('.js')).length, 'no built module shipped').toBeGreaterThan(0);
    // A map would point at `src`, which isn't published
    expect(shipped.filter((file) => file.endsWith('.map'))).toEqual([]);
  });

  it('exports declarations and code for every entry but its manifest, and no source condition', () => {
    // An export a consumer resolves to `src`, which isn't shipped, would fail them under that condition
    const conditions = Object.entries(readManifest(consumer().installed).exports).map(([key, target]) =>
      [key, typeof target === 'object' && target !== null ? Object.keys(target).sort().join() : typeof target]);
    expect(conditions.filter(([, keys]) => keys !== 'default,types')).toEqual([['./package.json', 'string']]);
    expect(codeExports(consumer())).toContain(MANIFEST.name);
  });

  const matrix = (['floor', 'current'] as const).flatMap((tsc) => MODULE_RESOLUTIONS.map((moduleResolution) => ({ tsc, moduleResolution })));

  it.each(matrix)('compiles for a consumer with no Node types under TypeScript $tsc, moduleResolution $moduleResolution', async ({ tsc, moduleResolution }) => {
    const dir = path.join(consumer().dir, `compile-${tsc}-${moduleResolution}`);
    writeFiles(dir, {
      'package.json': MODULE_MANIFEST,
      'tsconfig.json': JSON.stringify({ compilerOptions: consumerOptions(moduleResolution), include: ['*.ts'] }),
      // Every export, so one that resolves to nothing, or to code without declarations (`any` under noImplicitAny), fails
      'exports.ts': codeExports(consumer()).map((specifier, i) => `export * as m${i} from '${specifier}';`).join('\n'),
      'typed.ts': TYPED,
    });
    expect(await run(process.execPath, [compilers()[tsc], '-p', dir], dir)).toEqual({ code: 0, output: '' });
  });

  it('loads each export in a plain Node process, from its dist, with elmdb beside it', async () => {
    // A plain Node process resolves as a consumer's runtime does: no condition but Node's own
    const specifiers = codeExports(consumer());
    const { code, output } = await run(process.execPath, ['--input-type=module', '-e', `
      const loaded = [];
      for (const specifier of ${JSON.stringify(specifiers)}) loaded.push([import.meta.resolve(specifier), Object.keys(await import(specifier)).length]);
      process.stdout.write(JSON.stringify(loaded));
    `], consumer().dir);
    expect(code, output).toBe(0);
    const loaded = JSON.parse(output) as Array<[string, number]>;
    const dist = pathToFileURL(path.join(consumer().installed, 'dist')).href;
    expect(loaded).toHaveLength(specifiers.length);
    expect(loaded.filter(([url, exported]) => !url.startsWith(`${dist}/`) || exported === 0)).toEqual([]);
  });

  it('loads each export via CommonJS require() in Node, with elmdb beside it', async () => {
    const specifiers = codeExports(consumer());
    const { code, output } = await run(process.execPath, ['-e', `
      const loaded = [];
      for (const specifier of ${JSON.stringify(specifiers)}) {
        const mod = require(specifier);
        loaded.push([specifier, Object.keys(mod).length]);
      }
      process.stdout.write(JSON.stringify(loaded));
    `], consumer().dir);
    expect(code, output).toBe(0);
    const loaded = JSON.parse(output) as Array<[string, number]>;
    expect(loaded).toHaveLength(specifiers.length);
    expect(loaded.filter(([, exported]) => exported === 0)).toEqual([]);
  });

  it('exposes no any in any export', () => {
    expect(exportsWithAny(consumer().installed)).toEqual([]);
  });
});

/**
 * `<subpath> <export>` for each export of the package at `packageDir` whose type contains `any`, walking into its own
 * declarations wherever they're reached: an export's signature, its `this` parameter, public members, type arguments,
 * namespace members, conditional branches, and type parameter constraints or defaults, a few levels deep. A
 * dependency's types are its own business and aren't walked; the standard library's are, through the type arguments
 * given them.
 */
function exportsWithAny(packageDir: string): string[] {
  const root = fs.realpathSync(packageDir) + path.sep;
  const entries = Object.entries(readManifest(packageDir).exports)
    .filter(([, target]) => typeof target === 'object' && target !== null && 'types' in target)
    .map(([key, target]) => [key, path.join(packageDir, (target as { types: string }).types)] as const);
  const program = ts.createProgram(entries.map(([, file]) => file), {
    strict: true, skipLibCheck: true, noEmit: true, types: [], target: ts.ScriptTarget.ES2022, lib: ['lib.es2022.d.ts'],
    module: ts.ModuleKind.ESNext, moduleResolution: ts.ModuleResolutionKind.Bundler,
  });
  const checker = program.getTypeChecker();
  const standardLibrary = (node: ts.Node) => program.isSourceFileDefaultLibrary(node.getSourceFile());
  const thirdParty = (node: ts.Node) => {
    const file = node.getSourceFile().fileName;
    return /\/node_modules\//.test(file) && !standardLibrary(node) && !fs.realpathSync(file).startsWith(root);
  };
  const typeArguments = (type: ts.Type) => (type.flags & ts.TypeFlags.Object && (type as ts.TypeReference).target
    ? checker.getTypeArguments(type as ts.TypeReference)
    : []);

  /** `<T = any>` (a caller who leaves T out gets any) or `<T extends …any…>` (any is what T accepts and reads as) */
  function typeParameterLeaks(parameter: ts.TypeParameter, seen: Set<ts.Type>, depth: number): boolean {
    const fallback = checker.getDefaultFromTypeParameter(parameter);
    const constraint = checker.getBaseConstraintOfType(parameter);
    return [fallback, constraint].some((t) => t && containsAny(t, seen, depth + 1));
  }

  function containsAny(type: ts.Type, seen: Set<ts.Type>, depth: number): boolean {
    if (type.flags & ts.TypeFlags.Any) return true;
    if (seen.has(type) || depth > 6) return false;
    seen.add(type);
    const any = (types: readonly ts.Type[]) => types.some((t) => containsAny(t, seen, depth + 1));
    const alias = type.aliasSymbol?.declarations?.[0];
    const declaration = type.symbol?.declarations?.[0];
    if ((alias && thirdParty(alias)) || (declaration && thirdParty(declaration))) return false;
    if ((alias && standardLibrary(alias)) || (declaration && standardLibrary(declaration))) {
      return any([...(alias && standardLibrary(alias) ? type.aliasTypeArguments ?? [] : []), ...typeArguments(type)]);
    }
    if (type.isUnionOrIntersection()) return any(type.types);
    if (type.flags & ts.TypeFlags.TypeParameter) return typeParameterLeaks(type as ts.TypeParameter, seen, depth);
    if (type.flags & ts.TypeFlags.Conditional) {
      // A generic conditional keeps its branches unresolved until it's instantiated, so walk the written ones
      const { checkType, extendsType, root: { node } } = type as ts.ConditionalType;
      if (any([checkType, extendsType, checker.getTypeFromTypeNode(node.trueType), checker.getTypeFromTypeNode(node.falseType)])) return true;
    }
    if (type.flags & ts.TypeFlags.IndexedAccess) {
      const indexed = type as ts.IndexedAccessType;
      if (any([indexed.objectType, indexed.indexType])) return true;
    }
    for (const signature of [...type.getCallSignatures(), ...type.getConstructSignatures()]) {
      if (signature.getTypeParameters()?.some((p) => typeParameterLeaks(p, seen, depth))) return true;
      // `this: any` is a parameter a caller's receiver is checked against, and it isn't in getParameters()
      const parameters = [...(signature.thisParameter ? [signature.thisParameter] : []), ...signature.getParameters()];
      if (any([signature.getReturnType(), ...parameters.map((p) => checker.getTypeOfSymbol(p))])) return true;
    }
    if (!(type.flags & ts.TypeFlags.Object)) return false;
    if (any([...typeArguments(type), ...checker.getIndexInfosOfType(type).map((info) => info.type)])) return true;
    return any(type.getProperties().filter((property) => {
      const member = property.valueDeclaration ?? property.declarations?.[0];
      return !member || !(thirdParty(member) || ts.getCombinedModifierFlags(member as ts.Declaration) & (ts.ModifierFlags.Private | ts.ModifierFlags.Protected));
    }).map((property) => checker.getTypeOfSymbol(property)));
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
  const tempDir = tempDirs();

  /** A package `pkg` whose one entry declares `source`, beside a dependency `other` that declares `dependency` */
  function scan(source: string, dependency = ''): string[] {
    const root = tempDir('ears-any-detector-');
    writeFiles(root, {
      'node_modules/other/package.json': JSON.stringify({ name: 'other', types: './index.d.ts' }),
      'node_modules/other/index.d.ts': dependency,
      'pkg/package.json': JSON.stringify({ name: '@fixture/leaky', exports: { '.': { types: './index.d.ts', default: './index.js' } } }),
      'pkg/index.d.ts': source,
    });
    return exportsWithAny(path.join(root, 'pkg'));
  }

  it.each([
    ['a type-parameter default', 'export declare function f<T = any>(x: T): void;', '. f'],
    ['a type-parameter constraint', 'export declare function f<T extends { leak: any }>(x: T): void;', '. f'],
    ['a this parameter', 'export declare function f(this: any, x: number): void;', '. f'],
    ['a return type', 'export declare function f(): any;', '. f'],
    ['a generic conditional branch', 'export type f<T> = T extends string ? any : number;', '. f'],
    ['a public member', 'export interface f { leak: any }', '. f'],
    ['a type argument', 'export declare const f: Array<any>;', '. f'],
    ['an index signature', 'export interface f { [key: string]: any }', '. f'],
    ['a type-only namespace member, by its name', 'export declare namespace f { type Leak = any; }', '. f.Leak'],
  ])('catches %s', (_form, source, label) => {
    expect(scan(source)).toEqual([label]);
  });

  it('reports nothing for declarations that expose no any', () => {
    expect(scan(`
      export declare function fn<T extends { ok: string } = { ok: string }>(this: void, x: T): T;
      export interface Shape { value: unknown; nested: { list: readonly string[] } }
      export declare namespace NS { type Alias = string }
      export type Cond<T> = T extends string ? number : boolean;
    `)).toEqual([]);
  });

  it("leaves a dependency's type alone, its any being its own business", () => {
    expect(scan("import type { Theirs } from 'other';\nexport declare const f: Theirs;", 'export interface Theirs { leak: any }')).toEqual([]);
  });
});
