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
import { beforeAll, describe, expect, it } from 'vitest';
import {
  MANIFEST, MODULE_MANIFEST, MODULE_RESOLUTIONS, PACKAGE_DIR, consumerOptions, packedConsumer, writeFiles, type ModuleResolution,
} from '../_support/packed.ts';

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

const WRITES = ['set', 'append', 'removeAt'];

/** What the positions `at` complete: every name in `offers`, none in `not`, and nothing else when `only` */
const EXPECTED: Array<{ what: string; at: string[]; offers: string[]; not?: string[]; only?: true }> = [
  { what: "a read, the Note's fields", offers: ['title', 'pinned'], at: ['pick', 'pickOne', 'outPick', 'where', 'has', 'lacks', 'orderBy', 'distinct', 'groupBy', 'searchFields', 'recencyField', 'hitsFields', 'hybridFields', 'getAttr'] },
  { what: "a read, the engine's fields", at: ['where'], offers: ['createdAt'] },
  { what: "a write, the declared fields and not the engine's", at: WRITES, offers: ['title', 'pinned'], not: ['createdAt'] },
  // A required field is replaced with set, never deleted
  { what: 'a delete, the optional fields alone', at: ['delete'], offers: ['body'], not: ['title'] },
  { what: "a created row, the shape's fields and the identity it may bring", at: ['createRow'], offers: ['title', 'pinned', 'body', 'id', 'createdAt'], not: ['entityType'] },
  { what: 'the entity names', offers: ['Note', 'Folder'], at: ['query', 'newId', 'out', 'in', 'ancestors', 'descendants', 'both', 'ofType'] },
  { what: 'the relation kinds', offers: ['contains', 'cites'], at: ['outKind', 'linkKind', 'linkedToKind', 'ancestorsKind', 'descendantsKind', 'bothKind', 'pathToKind', 'degreesKind', 'rankKind', 'weakComponentsKind', 'strongComponentsKind', 'communitiesKind', 'topologicalKind', 'similarKind'] },
  { what: 'the roles', at: ['grantRole', 'withRole'], offers: ['starred', 'archived'] },
  { what: 'a weighted kind, a weight', at: ['weightedLinkOption'], offers: ['info', 'weight'] },
  { what: 'an unweighted kind, no weight', at: ['linkOption'], offers: ['info'], not: ['weight'] },
  { what: "a rank, its walk's options", at: ['rankOption'], offers: ['direction', 'seeds', 'weights'] },
  { what: 'a similarity measure, the measures', at: ['similarMeasure'], offers: ['adamicAdar', 'jaccard'], only: true },
  // A topological order follows each edge one way
  { what: 'a topological direction, one way', at: ['topologicalDirection'], offers: ['in', 'out'], only: true },
  { what: 'a ranked read, the rank indexes', at: ['rankedIndex'], offers: ['noteRank'], only: true },
];

/** Each typo in COMPLETIONS, and the name its error should list */
const TYPOS = [['titel', 'title'], ['pinnd', 'pinned'], ['containz', 'contains'], ['stared', 'starred']];

const consumer = packedConsumer();

/**
 * String completions at each marked position of COMPLETIONS, and that file's diagnostics, from a program beside the
 * consumer's `node_modules` that imports the package by its name: the typed schema, and the completions. The source
 * layout maps the name to `src`, as a program built with the package does.
 */
function completionsIn(layout: 'source' | 'packed', moduleResolution: ModuleResolution): { at: Record<string, string[]>; diagnostics: string[] } {
  const dir = path.join(consumer().dir, `completions-${layout}-${moduleResolution}`);
  const schema = fs.readFileSync(path.join(PACKAGE_DIR, 'tests', '_support', 'typed-schema.ts'), 'utf8');
  const relative = "'../../src/index.ts'";
  expect(schema, 'the typed schema no longer imports the package as this rewrites it').toContain(relative);
  const markers: Record<string, number> = {};
  let removed = 0;
  const completions = COMPLETIONS.replace(/\|(\w+)\|/g, (marker: string, name: string, offset: number) => {
    markers[name] = offset - removed;
    removed += marker.length;
    return '';
  });
  writeFiles(dir, { 'package.json': MODULE_MANIFEST, 'schema.ts': schema.replace(relative, `'${MANIFEST.name}'`), 'completions.ts': completions });

  const { options, errors } = ts.convertCompilerOptionsFromJson({
    ...consumerOptions(moduleResolution),
    ...(layout === 'source'
      ? { paths: { [MANIFEST.name]: [path.join(PACKAGE_DIR, 'src', 'index.ts')] }, allowImportingTsExtensions: true, skipLibCheck: true }
      : {}),
  }, dir);
  expect(errors.map((d) => ts.flattenDiagnosticMessageText(d.messageText, ' '))).toEqual([]);
  const file = path.join(dir, 'completions.ts');
  const { fileExists, readFile, readDirectory, directoryExists, getDirectories, realpath } = ts.sys;
  const service = ts.createLanguageService({
    getScriptFileNames: () => [path.join(dir, 'schema.ts'), file],
    getScriptVersion: () => '1',
    getScriptSnapshot: (name) => (fs.existsSync(name) ? ts.ScriptSnapshot.fromString(fs.readFileSync(name, 'utf8')) : undefined),
    getCurrentDirectory: () => dir,
    getCompilationSettings: () => options,
    getDefaultLibFileName: (settings) => ts.getDefaultLibFilePath(settings),
    fileExists, readFile, readDirectory, directoryExists, getDirectories, realpath,
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

  it.each(EXPECTED)('offers $what', ({ at: positions, offers, not = [], only }) => {
    const wrong = positions.filter((position) => {
      const offered = at[position] ?? [];
      return !offers.every((name) => offered.includes(name)) || not.some((name) => offered.includes(name))
        || (only && offered.length !== offers.length);
    });
    expect(Object.fromEntries(wrong.map((position) => [position, at[position]]))).toEqual({});
  });

  it.each(TYPOS)("lists, in the error for %s, %s", (typo, meant) => {
    expect(diagnostics.find((message) => message.includes(`"${typo}"`))).toMatch(`"${meant}"`);
  });
});
