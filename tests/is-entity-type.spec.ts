// isEntityType answers from the installed engine's registered entity types (the running app's), so a pack's step or
// action can check any registered pack's types, not only the ones its own generated facade knows
import { afterAll, describe, expect, it } from 'vitest';
import { createEarsEngine, installEngine, isEntityType } from '../src/index.ts';

afterAll(() => { installEngine(undefined); });

describe('isEntityType', () => {
  it("follows the installed engine's registered entity types", () => {
    const registered = new Set(['Note', 'Memo']);
    installEngine(createEarsEngine({ isEntityType: (name) => registered.has(name) }).query);
    expect(isEntityType('Memo')).toBe(true);
    expect(isEntityType('Nope')).toBe(false);
    registered.delete('Memo');
    expect(isEntityType('Memo')).toBe(false);
    installEngine(createEarsEngine({ isEntityType: () => true }).query);
    expect(isEntityType('Nope')).toBe(true);
  });
});
