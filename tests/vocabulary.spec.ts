// The engine's vocabulary is held by rules a member is classified by on its own — its type and its name — rather
// than by a list of members (`CLAUDE.md`, Vocabulary). A member added with a conventional name is checked the moment
// it compiles; one that breaks a convention fails here, and the only list is the exceptions, each with its reason.
// The API reports (`etc/*.api.md`) are where a change to the surface itself is reviewed.
import { describe, expect, expectTypeOf, it } from 'vitest';
import type { Ears, EarsAdmin, QueryBuilder, WriteBuilder } from '../src/index.ts';
import { freshEngine } from './engine/engine-under-test.ts';

type Signature = (...args: never[]) => unknown;
type Ret<F> = F extends Signature ? ReturnType<F> : never;
type Params<F> = F extends Signature ? Parameters<F> : never;
type Member<T> = keyof T & string;

/** `is*` members that don't answer a boolean */
type IsNotBoolean<T> = { [K in Member<T>]: K extends `is${string}` ? ([Ret<T[K]>] extends [boolean] ? never : K) : never }[Member<T>];
/** `find*` members that don't answer a list (a search may find nothing, so it answers an empty one) */
type FindNotList<T> = { [K in Member<T>]: K extends `find${string}` ? ([Ret<T[K]>] extends [readonly unknown[]] ? never : K) : never }[Member<T>];
/** Whether an argument is a boolean (optional or not): `unknown` and `any` take one but aren't one */
type IsFlag<P> = unknown extends P ? false : [Exclude<P, undefined>] extends [never] ? false : [Exclude<P, undefined>] extends [boolean] ? true : false;
/** Members taking a boolean argument: a direction or mode flag, where a step names the direction instead */
type TakesFlag<T> = { [K in Member<T>]: true extends { [I in keyof Params<T[K]>]: IsFlag<Params<T[K]>[I]> }[number] ? K : never }[Member<T>];
/** Write builder members that don't return the builder, so a chain of writes ends there */
type EndsChain<T> = { [K in Member<T>]: [Ret<T[K]>] extends [WriteBuilder] ? never : K }[Member<T>];

type QB = QueryBuilder<string>;
type WB = WriteBuilder;

describe('rules read off the types', () => {
  it('is* answers a boolean and find* a list, on every face', () => {
    expectTypeOf<IsNotBoolean<Ears>>().toBeNever();
    expectTypeOf<IsNotBoolean<QB>>().toBeNever();
    expectTypeOf<FindNotList<Ears>>().toBeNever();
    expectTypeOf<FindNotList<EarsAdmin>>().toBeNever();
    // The rules have members to hold: an empty population passes everything
    expectTypeOf<Extract<Member<Ears>, `is${string}`>>().not.toBeNever();
    expectTypeOf<Extract<Member<Ears>, `find${string}`>>().not.toBeNever();
  });

  it('no step or write takes a direction or mode flag', () => {
    expectTypeOf<TakesFlag<QB>>().toBeNever();
    expectTypeOf<TakesFlag<WB>>().toBeNever();
    expectTypeOf<TakesFlag<Ears>>().toBeNever();
  });

  it('writes chain; what ends a chain is the lifecycle end and the two reads of the builder', () => {
    // destroy leaves nothing to write to; id and row read the builder rather than writing
    expectTypeOf<EndsChain<WB>>().toEqualTypeOf<'destroy' | 'id' | 'row'>();
  });

  it('each rule names the member that breaks it', () => {
    interface Broken {
      isOpen(): string;
      findAll(): number;
      step(outward: boolean): void;
      set(): number;
    }
    expectTypeOf<IsNotBoolean<Broken>>().toEqualTypeOf<'isOpen'>();
    expectTypeOf<FindNotList<Broken>>().toEqualTypeOf<'findAll'>();
    expectTypeOf<TakesFlag<Broken>>().toEqualTypeOf<'step'>();
    expectTypeOf<EndsChain<Broken>>().toEqualTypeOf<Member<Broken>>();
  });
});

/** Each face's members, read off a real engine */
function faces(): Record<'engine' | 'admin' | 'query' | 'builder' | 'write', Record<string, unknown>> {
  const engine = freshEngine({ types: ['Note'], relationKinds: [], roles: [] });
  const id = engine.create('Note').id();
  const { admin, ...operations } = engine;
  const face = (members: object) => members as Record<string, unknown>;
  return { engine: face(operations), admin: face(admin), query: face(engine.query), builder: face(engine.query.all()), write: face(engine.write(id)) };
}

/** A negation without its positive, or a direction without its opposite */
function unpaired(names: readonly string[]): string[] {
  const has = (name: string) => names.includes(name);
  const twins = (name: string): string[] => [
    ...(/^un[a-z]/.test(name) ? [name.slice(2)] : []),
    ...(name.endsWith('To') ? [`${name.slice(0, -2)}From`] : []),
    ...(name.endsWith('From') ? [`${name.slice(0, -4)}To`] : []),
    ...(/^out(?=[A-Z]|$)/.test(name) ? [name.replace(/^out/, 'in')] : []),
    ...(/^in(?=[A-Z]|$)/.test(name) ? [name.replace(/^in/, 'out')] : []),
    ...({ ancestors: ['descendants'], descendants: ['ancestors'] }[name] ?? []),
  ];
  return names.filter((name) => twins(name).some((other) => !has(other)));
}

/** `get*` members that answer `null` for an id no entity has: absence is `undefined`, and `null` a stored value */
function absentAsNull(face: Record<string, unknown>): string[] {
  return Object.keys(face).filter((name) => /^get[A-Z]/.test(name))
    .filter((name) => (face[name] as (...args: unknown[]) => unknown)('Note-absent', 'title') === null);
}

/** The members that break a rule on purpose, each with why. One that no longer breaks it is reported as stale */
const UNPAIRED_BY_DESIGN: Record<string, string> = {
  unlinkAll: 'removes every edge of the entity; its positive is a link per edge, not one call',
};

describe('rules read off the names', () => {
  it('pairs every negation with its positive and every direction with its opposite', () => {
    const all = Object.values(faces()).flatMap((face) => unpaired(Object.keys(face)));
    expect(all.filter((name) => !Object.hasOwn(UNPAIRED_BY_DESIGN, name)), 'unpaired members').toEqual([]);
    expect(Object.keys(UNPAIRED_BY_DESIGN).filter((name) => !all.includes(name)), 'exceptions that excuse nothing').toEqual([]);
  });

  it('answers undefined, never null, from every get* asked about an id no entity has', () => {
    const { engine, admin } = faces();
    expect(Object.keys({ ...engine, ...admin }).filter((name) => /^get[A-Z]/.test(name)).length, 'get* members').toBeGreaterThan(3);
    expect(absentAsNull(engine)).toEqual([]);
    expect(absentAsNull(admin)).toEqual([]);
  });

  it('each rule names the member that breaks it', () => {
    expect(unpaired(['linkedTo', 'unlinkedTo', 'out', 'ancestors', 'outEdges', 'inEdges'])).toEqual(['linkedTo', 'unlinkedTo', 'out', 'ancestors']);
    expect(absentAsNull({ getThing: () => null, getOther: () => undefined, other: () => null })).toEqual(['getThing']);
  });
});
