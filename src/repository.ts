import { installedEngine } from './installed.ts';

const notRegistered = (name: string) => new Error(
  `[repository] "${name}" is not registered: a pack declares its repositories in apack.json (features[].repositories)`
);

/** An engine's repository registry */
export function createRepositoryRegistry() {
  const entries: Record<string, unknown> = {};

  function registerRepository(name: string, value: unknown): void {
    entries[name] = value;
  }

  function unregisterRepository(name: string): void {
    delete entries[name];
  }

  const repository: Record<string, unknown> = new Proxy({} as Record<string, unknown>, {
    get(_, prop) {
      if (typeof prop === 'symbol') return undefined;
      const value = entries[prop];
      if (value === undefined) throw notRegistered(prop);
      return value;
    },
  });

  return { repository, registerRepository, unregisterRepository, entries: () => ({ ...entries }) };
}

/** Registers a repository under a name. Packs declare theirs in apack.json (`features[].repositories`). */
export function registerRepository(name: string, value: unknown): void {
  installedEngine().registerRepository(name, value);
}

/** Removes a registered repository (its pack stopped) */
export function unregisterRepository(name: string): void {
  installedEngine().unregisterRepository(name);
}

/**
 * Every registered repository, for host code. Packs use the `repository` from their
 * `#generated/repository`, typed with their own and their dependencies' repositories.
 */
export const repository: Record<string, unknown> = /*#__PURE__*/ new Proxy({} as Record<string, unknown>, {
  get(_, prop) {
    if (typeof prop === 'symbol') return undefined;
    return installedEngine().repository[prop];
  },
});

export type Repository = typeof repository;
