/**
 * Editor completions over a typed facade, from the TypeScript language service. EARS' types can compile and pass every
 * type assertion while the suggestions an editor offers disappear: a field parameter wrapped in a conditional or
 * mapped type still checks, and no longer completes (`runtime.ts`, `QueryBuilder`). So each position a program types a
 * name at is asked directly, over `_support/typed-schema.ts`'s schema, in the two layouts a program meets: the package's
 * source, and the package as npm publishes it, whose declarations are the build's.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import ts from 'typescript';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { MANIFEST, MODULE_RESOLUTIONS, PACKAGE_DIR, consumerOptions, installPacked, type Consumer, type ModuleResolution } from '../_support/packed.ts';

/** `|name|` marks a position; the marker is removed and the position asked for completions */
const COMPLETIONS = `
import { create, getAttr, newId, query, write, type EARS } from './schema.js';
declare const noteId: EARS.EntityId<'Note'>;
query('Note').pick(['|pick|']);
query('Note').pickOne(['|pickOne|']);
query.ids(noteId).out('cites', 'Note').pick(['|outPick|']);
query('Note').where('|where|');
query('Note').has('|has|');
query('Note').lacks('|lacks|');
query('Note').orderBy('|orderBy|');
query('Note').distinct('|distinct|');
query('Note').groupBy('|groupBy|');
query('Note').search('x', { fields: ['|searchFields|'] });
query('Note').search('x', { recency: { field: '|recencyField|', halfLife: 1 } });
query('Note').search('x').hits({ fields: ['|hitsFields|'] });
query('Note').hybrid('x', [1], { fields: ['|hybridFields|'] });
getAttr(noteId, '|getAttr|');
newId('|newId|');
query.ids(noteId).out('cites', '|out|');
query.ids(noteId).in('cites', '|in|');
query.ids(noteId).ancestors('contains', '|ancestors|');
query.ids(noteId).descendants('contains', '|descendants|');
query.ids(noteId).both('cites', '|both|');
query.all().ofType('|ofType|');
query('|query|');
query('Note').has('titel');
write(noteId).set('|set|', true);
write(noteId).append('|append|', true);
write(noteId).delete('|delete|');
write(noteId).removeAt('|removeAt|', 0);
write(noteId).set('pinnd', true);
query.ids(noteId).out('|outKind|');
query.ids(noteId).ancestors('|ancestorsKind|');
query.ids(noteId).descendants('|descendantsKind|');
query.ids(noteId).both('|bothKind|');
query.ids(noteId).pathTo(noteId, '|pathToKind|');
query.ids(noteId).degrees('|degreesKind|');
query('Note').rank('|rankKind|');
query('Note').weakComponents('|weakComponentsKind|');
query('Note').strongComponents('|strongComponentsKind|');
query('Note').communities('|communitiesKind|');
query('Note').topological('|topologicalKind|');
query('Note').similar(noteId, '|similarKind|');
query('Note').similar(noteId, 'cites', { measure: '|similarMeasure|' });
query('Note').topological('contains', { direction: '|topologicalDirection|' });
write(noteId).link('|linkKind|', noteId);
write(noteId).link('cites', noteId, { |weightedLinkOption| });
write(noteId).link('contains', noteId, { |linkOption| });
query('Note').rank('contains', { |rankOption| });
query('Note').ranked('|rankedIndex|');
query('Note').linkedTo('|linkedToKind|', noteId);
write(noteId).link('containz', noteId);
write(noteId).grant('|grantRole|');
query.all().withRole('|withRole|');
write(noteId).grant('stared');
create('Note', { |createRow| });
`;
/** Positions taking a Note field */
const FIELD_POSITIONS = ['pick', 'pickOne', 'outPick', 'where', 'has', 'lacks', 'orderBy', 'distinct', 'groupBy', 'searchFields', 'recencyField', 'hitsFields', 'hybridFields', 'getAttr'];
/** Positions taking a field a write may name: the declared ones, without the engine's own (`createdAt`, `updatedAt`, …) */
const WRITE_POSITIONS = ['set', 'append', 'removeAt'];
const KIND_POSITIONS = ['outKind', 'linkKind', 'linkedToKind', 'ancestorsKind', 'descendantsKind', 'bothKind', 'pathToKind', 'degreesKind', 'rankKind', 'weakComponentsKind', 'strongComponentsKind', 'communitiesKind', 'topologicalKind', 'similarKind'];
const ROLE_POSITIONS = ['grantRole', 'withRole'];
const NAME_POSITIONS = ['query', 'newId', 'out', 'in', 'ancestors', 'descendants', 'both', 'ofType'];

type Layout = 'source' | 'packed';

let consumer: Consumer;
beforeAll(async () => {
  consumer = await installPacked();
});
afterAll(() => consumer?.remove());

/**
 * A program in its own directory beside the consumer's `node_modules`: the typed schema, importing the package by its
 * name, and the completions file. The source layout maps the name to `src`, as a program built with the package does.
 */
function writeProgram(layout: Layout, moduleResolution: ModuleResolution): { dir: string; options: ts.CompilerOptions; files: string[] } {
  const dir = path.join(consumer.dir, `completions-${layout}-${moduleResolution}`);
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify({ name: 'consumer-completions', private: true, type: 'module' }));
  const schema = fs.readFileSync(path.join(PACKAGE_DIR, 'tests', '_support', 'typed-schema.ts'), 'utf8');
  const relative = "'../../src/index.ts'";
  expect(schema, 'the typed schema no longer imports the package as this rewrites it').toContain(relative);
  fs.writeFileSync(path.join(dir, 'schema.ts'), schema.replace(relative, `'${MANIFEST.name}'`));
  const options = {
    ...consumerOptions(moduleResolution),
    ...(layout === 'source'
      ? { paths: { [MANIFEST.name]: [path.join(PACKAGE_DIR, 'src', 'index.ts')] }, allowImportingTsExtensions: true, skipLibCheck: true }
      : {}),
  };
  const parsed = ts.convertCompilerOptionsFromJson(options, dir);
  expect(parsed.errors.map((d) => ts.flattenDiagnosticMessageText(d.messageText, ' '))).toEqual([]);
  return { dir, options: parsed.options, files: [path.join(dir, 'schema.ts')] };
}

/** String completions at each marked position of COMPLETIONS, and the file's diagnostics */
function completionsIn(layout: Layout, moduleResolution: ModuleResolution): { at: Record<string, string[]>; diagnostics: string[] } {
  const { dir, options, files } = writeProgram(layout, moduleResolution);
  const file = path.join(dir, 'completions.ts');
  const markers: Record<string, number> = {};
  let removed = 0;
  const text = COMPLETIONS.replace(/\|(\w+)\|/g, (marker: string, name: string, offset: number) => {
    markers[name] = offset - removed;
    removed += marker.length;
    return '';
  });
  fs.writeFileSync(file, text);
  const service = ts.createLanguageService({
    getScriptFileNames: () => [...files, file],
    getScriptVersion: () => '1',
    getScriptSnapshot: (name) => (fs.existsSync(name) ? ts.ScriptSnapshot.fromString(fs.readFileSync(name, 'utf8')) : undefined),
    getCurrentDirectory: () => dir,
    getCompilationSettings: () => options,
    getDefaultLibFileName: (settings) => ts.getDefaultLibFilePath(settings),
    fileExists: ts.sys.fileExists,
    readFile: ts.sys.readFile,
    readDirectory: ts.sys.readDirectory,
    directoryExists: ts.sys.directoryExists,
    getDirectories: ts.sys.getDirectories,
    realpath: ts.sys.realpath,
  });
  const at = Object.fromEntries(Object.entries(markers).map(([name, position]) => [
    name,
    (service.getCompletionsAtPosition(file, position, {})?.entries ?? [])
      .filter((entry) => entry.kind === ts.ScriptElementKind.string || entry.kind === ts.ScriptElementKind.memberVariableElement)
      .map((entry) => entry.name),
  ]));
  const diagnostics = service.getSemanticDiagnostics(file).map((d) => ts.flattenDiagnosticMessageText(d.messageText, ' '));
  service.dispose();
  return { at, diagnostics };
}

const cases = (['source', 'packed'] as const).flatMap((layout) => MODULE_RESOLUTIONS.map((moduleResolution) => ({ layout, moduleResolution })));

describe.each(cases)('over the $layout package, under moduleResolution $moduleResolution', ({ layout, moduleResolution }) => {
  let at: Record<string, string[]>;
  let diagnostics: string[];
  beforeAll(() => {
    ({ at, diagnostics } = completionsIn(layout, moduleResolution));
  });
  const missing = (positions: string[], expected: string[]) => positions.filter((position) => !expected.every((name) => at[position]?.includes(name)));

  it('offers the fields and the entity names', () => {
    expect(missing(FIELD_POSITIONS, ['title', 'pinned']), 'positions without Note field completions').toEqual([]);
    expect(missing(NAME_POSITIONS, ['Note', 'Folder']), 'positions without entity-name completions').toEqual([]);
    expect(at.query, 'no completion for a type the schema does not declare').not.toContain('Unregistered');
    expect(at.where, "a read offers the engine's fields").toContain('createdAt');
  });

  it('offers a write the fields it may name, and a delete the optional ones alone', () => {
    expect(missing(WRITE_POSITIONS, ['title', 'pinned']), 'write positions without Note field completions').toEqual([]);
    expect(WRITE_POSITIONS.filter((position) => at[position]?.includes('createdAt')), 'write positions offering an engine field').toEqual([]);
    // A required field is replaced with set, never deleted
    expect(at.delete).toContain('body');
    expect(at.delete).not.toContain('title');
  });

  it('offers the relation kinds and roles, and an option naming one of a few modes those alone', () => {
    expect(missing(KIND_POSITIONS, ['contains', 'cites']), 'positions without relation-kind completions').toEqual([]);
    expect(missing(ROLE_POSITIONS, ['starred', 'archived']), 'positions without role completions').toEqual([]);
    expect([...at.similarMeasure!].sort()).toEqual(['adamicAdar', 'jaccard']);
    // A topological order follows each edge one way
    expect([...at.topologicalDirection!].sort()).toEqual(['in', 'out']);
    expect(at.rankedIndex).toEqual(['noteRank']);
  });

  it("offers a created row the shape's fields and the identity it may bring, and none of the engine's others", () => {
    expect(at.createRow).toEqual(expect.arrayContaining(['title', 'pinned', 'body', 'id', 'createdAt']));
    expect(at.createRow).not.toContain('entityType');
  });

  it('offers a weight to a weighted kind alone, and a rank its walk\'s options', () => {
    expect(at.weightedLinkOption).toEqual(expect.arrayContaining(['info', 'weight']));
    expect(at.linkOption).toContain('info');
    expect(at.linkOption).not.toContain('weight');
    expect(at.rankOption).toEqual(expect.arrayContaining(['direction', 'seeds', 'weights']));
  });

  it("lists, in a typo's error, the names it could have been", () => {
    expect(diagnostics.find((message) => message.includes('"titel"'))).toMatch(/"title"/);
    expect(diagnostics.find((message) => message.includes('"pinnd"'))).toMatch(/"pinned"/);
    expect(diagnostics.find((message) => message.includes('"containz"'))).toMatch(/"contains"/);
    expect(diagnostics.find((message) => message.includes('"stared"'))).toMatch(/"starred"/);
  });
});
