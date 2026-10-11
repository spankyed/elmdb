// The engine is an instance: no module in @apack/ears keeps data at module scope. The installed engine (installed.ts)
// is the one exception: a reference to an engine, not data.
import * as fs from 'node:fs';
import * as path from 'node:path';
import ts from 'typescript';
import { describe, expect, it } from 'vitest';

const SRC = path.resolve(import.meta.dirname, '..', 'src');
const ALLOWED: Record<string, string> = {
  'installed.ts:installed': 'the installed engine: a reference, not data',
};

const files = (dir: string): string[] => fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
  const full = path.join(dir, entry.name);
  return entry.isDirectory() ? files(full) : entry.name.endsWith('.ts') ? [full] : [];
});

const STATE_CONSTRUCTORS = new Set(['Map', 'Set', 'WeakMap', 'WeakSet', 'Array']);

/** Whether an initializer creates a container to fill: `new Map()` (and the like), `[]` or `{}` */
function isContainer(node: ts.Expression | undefined): boolean {
  if (!node) return false;
  if (ts.isAsExpression(node) || ts.isSatisfiesExpression(node) || ts.isParenthesizedExpression(node)) return isContainer(node.expression);
  if (ts.isNewExpression(node)) return ts.isIdentifier(node.expression) && STATE_CONSTRUCTORS.has(node.expression.text);
  if (ts.isArrayLiteralExpression(node)) return node.elements.length === 0;
  if (ts.isObjectLiteralExpression(node)) return node.properties.length === 0;
  return false;
}

/** Module-scope state in a source: `let`/`var` declarations and containers, in the file and its namespaces */
export function moduleState(file: string, text: string): string[] {
  const source = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true);
  const found: string[] = [];
  const visit = (statements: ts.NodeArray<ts.Statement>) => {
    for (const statement of statements) {
      if (ts.isModuleDeclaration(statement) && statement.body && ts.isModuleBlock(statement.body)) visit(statement.body.statements);
      if (!ts.isVariableStatement(statement)) continue;
      const mutable = !(statement.declarationList.flags & ts.NodeFlags.Const);
      for (const declaration of statement.declarationList.declarations) {
        if (mutable || isContainer(declaration.initializer)) found.push(declaration.name.getText(source));
      }
    }
  };
  visit(source.statements);
  return found;
}

describe('@apack/ears module state', () => {
  it('no engine module holds data at module scope', () => {
    const found = files(SRC).flatMap((file) => {
      const rel = path.relative(SRC, file);
      return moduleState(file, fs.readFileSync(file, 'utf-8')).map((name) => `${rel}:${name}`);
    }).filter((entry) => !(entry in ALLOWED));
    expect(found, 'keep state in the engine (createEarsEngine) or the object a factory returns').toEqual([]);
  });

  it('finds let, var and container state', () => {
    const text = [
      'let a = 1;', 'var b;', 'const c = new Map<string, number>();', 'const d = new Set();', 'const e: Record<string, unknown> = {};',
      'const f = [] as string[];', 'export const g = { x: 1 };', 'const h = () => new Map();', 'namespace N { let i = 0; }',
      'function k() { let inner = 0; const m = new Map(); return inner + m.size; }',
    ].join('\n');
    expect(moduleState('x.ts', text)).toEqual(['a', 'b', 'c', 'd', 'e', 'f', 'i']);
  });
});
