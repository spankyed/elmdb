// The port held to the binding, member by member: each member of `EarsDatabase` and `EarsUnit` answers exactly what
// the binding's `Database` and `Unit` answer, its bytes (`Buffer`) read as a `Uint8Array`. Assignment alone holds less:
// any answer is assignable to `void`, so a member of the binding turning into a promise would still compile. Types
// only, read by the typecheck (`npm run typecheck:ears`) and left out of the build.
import type { Database, Unit } from 'elmdb';
import type { EarsDatabase, EarsUnit } from '../database.ts';
import type { openDatabase } from './database.ts';

/** Whether `A` and `B` are one type, rather than each assignable to the other */
type Same<A, B> = (<T>() => T extends A ? 1 : 2) extends (<T>() => T extends B ? 1 : 2) ? true : false;

/** A binding's answer as the port declares it: bytes a `Uint8Array`, a unit the port's, inside a promise too */
type AsPort<T> = T extends Buffer ? Uint8Array
  : T extends Promise<infer U> ? Promise<AsPort<U>>
    : T extends Unit ? EarsUnit
      : T;

/** What a member answers: a method's return, or a property's value */
type Answer<M> = M extends (...args: never) => infer R ? R : M;

/** Per member of the port, `true`, or both answers where they differ, or that the binding has no such member */
type Held<Port, Binding> = {
  [K in keyof Port]-?: K extends keyof Binding
    ? Same<Answer<Port[K]>, AsPort<Answer<Binding[K]>>> extends true ? true : { port: Answer<Port[K]>; binding: Answer<Binding[K]> }
    : 'no member of the binding';
};

/** Fails to compile unless every member is `true` */
type Every<T extends Record<string, true>> = T;

export type DatabaseHeld = Every<Held<EarsDatabase, Database>>;
export type UnitHeld = Every<Held<EarsUnit, Unit>>;
/**
 * The binding's `Database` is an `EarsDatabase` as it is, which is what `openDatabase` answers with no cast, and so
 * what an engine takes; `openDatabase` declares the port, so its declarations name nothing of the binding
 */
export type OpenedHeld = Every<{
  port: Database extends EarsDatabase ? true : false;
  answer: Same<ReturnType<typeof openDatabase>, EarsDatabase>;
}>;
