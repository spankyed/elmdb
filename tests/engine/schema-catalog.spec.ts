import { describe, expect, it } from 'vitest';
import { engine } from './helpers.ts';

describe('catalog', () => {
  it('answers what the engine holds, and a watch hears only what changes it', async () => {
    const e = engine();
    const before = e.catalog();
    expect(before.types.find((type) => type.name === 'Task')?.count).toBe(0);
    expect(before.types.map((type) => type.name), 'the engine\'s own types are no type a query can name').not.toContain('Relation');

    const a = e.create('Task').set('title', 'a').id();
    const b = e.create('Task').set('title', 'b').id();
    e.write(a).link('blocks', b);
    const held = e.catalog();
    expect(held.types.find((type) => type.name === 'Task')).toMatchObject({ count: 2 });
    expect(held.types.find((type) => type.name === 'Task')?.fields).toContain('title');
    expect(held.relations.find((relation) => relation.kind === 'blocks')?.count).toBe(1);

    const delivered: unknown[] = [];
    const watching = e.watch(() => e.catalog(), (value) => delivered.push(value));
    await new Promise((resolve) => setTimeout(resolve, 20));
    const opened = delivered.length;
    e.write(a).set('title', 'renamed');
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(delivered, 'a new value of a registered field changes nothing').toHaveLength(opened);
    e.create('Task').set('title', 'c').id();
    await expect.poll(() => delivered.length).toBeGreaterThan(opened);
    watching.stop();
  });
});
