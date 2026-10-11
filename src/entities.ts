/*───────────────────────────────────────────────────────────────────────────
 * EARS type infrastructure
 *
 * The engine's structural type framework — Entity, EntityId, RelKind,
 * AttrKind, Blueprint, BaseEntity, etc. — with open types that the SDK and
 * packs compose with concrete values.
 *
 * Entity and RelKind are open by design:
 *   Entity  — the engine owns Relation; the SDK adds its entities
 *             (@apack/sdk's EARS) and packs declare theirs.
 *   RelKind — the engine provides the Custom() helper and open type; the SDK
 *             and packs add their relation kinds.
 *
 * CHANGE CONTROL: these types are a specified contract, and editor completions and error messages
 * depend on their exact form. Don't change them to make one call site compile; fix the call site.
 * Read packages/apack-sdk/TYPED-EARS.md (the contract and the pre-change checklist) first.
 *───────────────────────────────────────────────────────────────────────────*/

export namespace EARS {
  // ─── Entity ────────────────────────────────────────────────────────────
  // The engine's own entity; the SDK and packs add theirs
  export const Entity = {
    /** Every link between two entities */
    Relation: 'Relation',
  } as const;

  export type Entity = typeof Entity[keyof typeof Entity] | (string & {});
  /**
   * An entity id, tagged with its entity type when that's known (`EntityId<'Note'>`). A plain id
   * (`EntityId`) carries no tag, so it's accepted wherever a tagged one is expected; only an id tagged
   * with a different entity type is rejected.
   */
  export type EntityId<E extends string = string> =
    string extends E ? `${string}-${string}` : `${string}-${string}` & { readonly __entity?: E };

  // ─── RelKind ───────────────────────────────────────────────────────────
  // Concrete values come from the SDK and packs; the engine provides just the type framework.
  const _relCustom = <T extends string>(k: T) => k as T & RelKind;

  export const RelKind = {
    Custom: _relCustom,
  } as const;

  export type RelKind = string & {};

  // ─── Relations ─────────────────────────────────────────────────────────
  export interface RelationDetail {
    sourceEntity : EntityId;
    targetEntity : EntityId;
    relationType : RelKind;
    info?        : AttributeValue;
  }

  // ─── RoleKind ──────────────────────────────────────────────────────────
  const _roleCustom = <T extends string>(k: T) => k as T & RoleKind;

  export const RoleKind = {
    Custom: _roleCustom,
  } as const;

  export type RoleKind = string & {};

  // ─── AttrKind ──────────────────────────────────────────────────────────
  export const AttrKindValues = {
    Role            : 'role',
    RelationDetails : 'relationDetails',
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
    [AttrKindValues.RelationDetails] : RelationDetail;
    [key: string]                    : unknown;
  }

  // biome-ignore lint/suspicious/noExplicitAny: generic fallback
  export type AttributeValue<K extends AttrKind = AttrKind> = K extends keyof AttributePayloads ? AttributePayloads[K] : unknown;

  export type AttributeTypeMap = Record<EntityId, AttributeValue[]>;

  export type AttributeType  = AttrKind;
  export type AttributeStore = Record<string, AttributeTypeMap>;

  // ─── Blueprint ─────────────────────────────────────────────────────────
  export type Blueprint = {
    entity : EARS.Entity;
    attrs? : Record<string, unknown>;
    roles? : EARS.RoleKind[];
    uniqueRoles? : EARS.RoleKind[];
    rels?  : { kind: EARS.RelKind; target: Blueprint | EARS.EntityId; info?: unknown }[];
  };
}

export interface BaseEntity {
  id: EARS.EntityId;
  entityType: EARS.Entity;
  createdAt: number;
  updatedAt?: number;
}

/**
 * Entity type name → attribute shape. There is no global registry: each pack's
 * `#generated/ears` defines its `PackShapes` (its own entities plus its dependencies')
 * and exports EARS helpers typed against it.
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
 * An entity name parameter checked against the names `N` a pack declares; `E` is inferred from the
 * argument. A literal must be one of `N`: an undeclared name isn't a registered type at runtime, so
 * it matches nothing. A name only known at runtime (typed `string` or the open `EARS.Entity`) passes
 * unchecked, and so does everything when `N` is `string`.
 *
 * TypeScript can't check a name whose type is an unresolved type parameter, so a generic helper
 * constrains it to the pack's `EntityName`. Its callers get the rows typed with the entity's shape;
 * inside the helper only the fields every entity has are known:
 *
 * ```ts
 * import { findAll, type EntityName } from '#generated/ears';
 *
 * function newestOf<E extends EntityName>(entityType: E) {
 *   return findAll(entityType).sort((a, b) => b.createdAt - a.createdAt);
 * }
 * newestOf('Note')[0].title; // typed with the Note shape
 * ```
 *
 * A helper over any string (`<E extends string>`) is rejected; pass `entityType as string` to
 * opt out of the check.
 */
export type EntityNameArg<N extends string, E extends string> =
  string extends E ? E : [E] extends [N] ? E : N;
