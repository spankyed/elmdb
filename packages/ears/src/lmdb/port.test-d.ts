// Compile-time proof that elmdb's native `Database` satisfies `@elmdb/ears`'s `EarsDatabase` port
// member by member with zero casts and exact return-type parity.
import type { Database } from 'elmdb';
import type { EarsDatabase } from '../database.ts';
import type { openDatabase } from './database.ts';

type Same<A, B> = (<T>() => T extends A ? 1 : 2) extends (<T>() => T extends B ? 1 : 2) ? true : false;

type Answer<M> = M extends (...args: never) => infer R ? R : M;

type Held<Port, Binding> = {
  [K in keyof Port]-?: K extends keyof Binding
    ? Same<Answer<Port[K]>, Answer<Binding[K]>> extends true
      ? true
      : { port: Answer<Port[K]>; binding: Answer<Binding[K]> }
    : 'no member of the binding';
};

type Every<T extends Record<string, true>> = T;

export type DatabaseHeld = Every<Held<EarsDatabase, Database>>;

export type OpenedHeld = Every<{
  port: Database extends EarsDatabase ? true : false;
  answer: Same<ReturnType<typeof openDatabase>, EarsDatabase>;
}>;
