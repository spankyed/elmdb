
const notRegistered = (name: string) => new Error(
  `[repository] "${name}" is not registered: a pack declares its repositories in apack.json (features[].repositories)`
);

/** An engine's repository registry */
export function createRepositoryRegistry() {
  const entries = new Map<string, unknown>();

  function registerRepository(name: string, value: unknown): void {
    entries.set(name, value);
  }

  function unregisterRepository(name: string): void {
    entries.delete(name);
  }

  const repository: Record<string, unknown> = new Proxy({} as Record<string, unknown>, {
    get(_, prop) {
      if (typeof prop === 'symbol') return undefined;
      const value = entries.get(prop);
      if (value === undefined) throw notRegistered(prop);
      return value;
    },
  });

  return { repository, registerRepository, unregisterRepository, entries: () => Object.fromEntries(entries) };
}
