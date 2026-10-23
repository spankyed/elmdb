import type { Create } from './write.ts';
import type { Query } from './query.ts';
import { EARS } from './entities.ts';

export interface Blueprint {
  entity: EARS.Entity;
  attrs?: Record<string, unknown>;
  roles?: EARS.RoleKind[];
  uniqueRoles?: EARS.RoleKind[];
  rels?: Array<{
    kind: EARS.RelKind;
    target: Blueprint | EARS.EntityId;
    info?: unknown;
  }>;
}

/** Fluent builder for a Blueprint */
export interface BlueprintBuilder {
  attr(k: string, v: unknown): BlueprintBuilder;
  grant(r: EARS.RoleKind): BlueprintBuilder;
  ensure(r: EARS.RoleKind): BlueprintBuilder;
  link(kind: EARS.RelKind, target: Blueprint | EARS.EntityId, info?: unknown): BlueprintBuilder;
  build(): Blueprint;
}

export const bp = (entity: EARS.Entity): BlueprintBuilder => {
  const b: Blueprint = { entity };

  return {
    attr(k: string, v: unknown) {
      (b.attrs ??= {})[k] = v;
      return this;
    },
    grant(r: EARS.RoleKind) {
      (b.roles ??= []).push(r);
      return this;
    },
    ensure(r: EARS.RoleKind) {
      (b.uniqueRoles ??= []).push(r);
      return this;
    },
    link(kind: EARS.RelKind, target: Blueprint | EARS.EntityId, info?: unknown) {
      (b.rels ??= []).push({ kind, target, info });
      return this;
    },
    build() {
      return b;
    },
  };
};

/** An engine's blueprint spawning, over its writes */
export function createSpawn({ create, query }: { create: Create; query: Query }) {
  function spawn(
    root: Blueprint,
    { dedupe = true } = {},
  ): EARS.EntityId {
    const cache = dedupe ? new Map<Blueprint, EARS.EntityId>() : undefined;

    const go = (node: Blueprint): EARS.EntityId => {
      if (cache?.has(node)) {
        return cache.get(node)!;
      }
      const builder = create(node.entity, node.attrs);
      const id = builder.id();
      cache?.set(node, id);

      for (const r of node.roles ?? []) {
        builder.grant(r);
      }
      for (const r of node.uniqueRoles ?? []) {
        builder.grantExclusive(r, query.all().ids());
      }
      for (const { kind, target, info } of node.rels ?? []) {
        const tgtId =
          typeof target === "object" ? go(target as Blueprint) : target;
        builder.link(kind, tgtId, { info, replace: true });
      }

      return id;
    };

    return go(root);
  }
  return spawn;
}
