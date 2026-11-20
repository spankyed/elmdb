/*───────────────────────────────────────────────────────────────────────────
 * EARS type infrastructure
 *
 * The engine's structural type framework — Entity, EntityId, RelKind,
 * AttrKind, Blueprint, BaseEntity, etc. — with open types that a program using
 * the engine composes with concrete values.
 *
 * Entity and RelKind are open by design:
 *   Entity  — the engine owns no entity type (a relation is not an entity);
 *             the program declares its own.
 *   RelKind — the engine provides the Custom() helper and open type; the program
 *             adds its relation kinds.
 *
 * CHANGE CONTROL: these types are a specified contract, and editor completions and error messages
 * depend on their exact form. Don't change them to make one call site compile; fix the call site.
 * docs/architecture/ears.md, Typed contract, says what checks a change to them.
 *───────────────────────────────────────────────────────────────────────────*/

export namespace EARS {
  // ─── Entity ────────────────────────────────────────────────────────────
  // An entity type's name. The engine declares none: a program declares its own, each as an `Entity` constant
  export type Entity = string & {};
  /**
   * An entity id, tagged with its entity type when that's known (`EntityId<'Note'>`). A plain id
   * (`EntityId`) carries no tag, so it's accepted wherever a tagged one is expected; only an id tagged
   * with a different entity type is rejected.
   */
  export type EntityId<E extends string = string> =
    string extends E ? `${string}-${string}` : `${string}-${string}` & { readonly __entity?: E };
  /**
   * A relation's id: read it with `getRelation`, change it with `updateRelation`, remove it with `removeRelation`.
   * `write` takes an entity's id and refuses this one, a relation being no entity. The engine hands one out from every
   * relation read (`findRelations`, `outEdges`, `inEdges`) and from `admin.addRelation`
   */
  export type RelationId = `${string}-${string}` & { readonly __relation: true };

  // ─── RelKind ───────────────────────────────────────────────────────────
  // Concrete values come from the program using the engine; the engine provides just the type framework.
  const _relCustom = <T extends string>(k: T) => k as T & RelKind;

  export const RelKind = {
    Custom: _relCustom,
  } as const;

  export type RelKind = string & {};

  // ─── RoleKind ──────────────────────────────────────────────────────────
  const _roleCustom = <T extends string>(k: T) => k as T & RoleKind;

  export const RoleKind = {
    Custom: _roleCustom,
  } as const;

  export type RoleKind = string & {};

  // ─── AttrKind ──────────────────────────────────────────────────────────
  export const AttrKindValues = {
    Role            : 'role',
  } as const;

  const _attrCustom = <T extends string>(k: T) => k as T & AttrKind;

  export const AttrKind = {
    ...AttrKindValues,
    Custom: _attrCustom,
  } as const;

  export type AttrKind = typeof AttrKindValues[keyof typeof AttrKindValues] | (string & {});

  // ─── Attribute payloads ────────────────────────────────────────────────
  export interface AttributePayloads {
    [AttrKindValues.Role]            : RoleKind;
    [key: string]                    : unknown;
  }

  // biome-ignore lint/suspicious/noExplicitAny: generic fallback
  export type AttributeValue<K extends AttrKind = AttrKind> = K extends keyof AttributePayloads ? AttributePayloads[K] : unknown;

  export type AttributeTypeMap = Record<EntityId, AttributeValue[]>;

  export type AttributeType  = AttrKind;
  export type AttributeStore = Record<string, AttributeTypeMap>;

}

export interface BaseEntity {
  id: EARS.EntityId;
  entityType: EARS.Entity;
  createdAt: number;
  updatedAt?: number;
  /** When the entity was put in the trash: only a read that reaches the trash (`query.withTrashed`, `query.trashed`) finds one */
  trashedAt?: number;
}

/**
 * Entity type name → attribute shape. There is no global registry: each schema a typed facade is
 * defined over (`defineEars`) names its own shapes, and the facade's helpers are typed against them.
 */
export type EntityShapes = { [entityType: string]: object };

/**
 * An entity type's shape in `S`. A type `S` doesn't declare reads as its base fields plus
 * `unknown` values: never `any`, so undeclared data has to be narrowed before use.
 *
 * The check is wrapped in tuples to make it NON-distributive. A naked conditional
 * distributes over a union `E`, and because `keyof Record<string, any>` is
 * `string | number`, `keyof` of a union with any undeclared arm collapses to roughly
 * `keyof BaseEntity` — over-constraining every caller whose `E` is not a single
 * declared literal.
 */
export type ShapeOf<S extends EntityShapes, E extends string> =
  [E] extends [keyof S]
    ? S[E] & BaseEntity & { id: EARS.EntityId<E> }
    : BaseEntity & Record<string, unknown>;

/**
 * An entity name parameter checked against the names `N` a schema declares; `E` is inferred from the
 * argument. A literal must be one of `N`: an undeclared name isn't a registered type at runtime, so
 * it matches nothing. A name only known at runtime (typed `string` or the open `EARS.Entity`) passes
 * unchecked, and so does everything when `N` is `string`.
 *
 * TypeScript can't check a name whose type is an unresolved type parameter, so a generic helper
 * constrains it to the schema's entity names. Its callers get the rows typed with the entity's shape;
 * inside the helper only the fields every entity has are known:
 *
 * ```ts
 * const { query } = defineEars<Schema>(() => engine);
 * type EntityName = Schema['names'];
 *
 * function newestOf<E extends EntityName>(entityType: E) {
 *   return query(entityType).pickAll().sort((a, b) => b.createdAt - a.createdAt);
 * }
 * newestOf('Note')[0].title; // typed with the Note shape
 * ```
 *
 * A helper over any string (`<E extends string>`) is rejected; pass `entityType as string` to
 * opt out of the check.
 */
export type EntityNameArg<N extends string, E extends string> =
  string extends E ? E : [E] extends [N] ? E : N;
