import { EarsError } from './errors.ts';

/**
 * The checks on the names an engine stores beside entity types, against what its creator registered. A name the
 * engine has never heard of is refused where it is used — by a write, which would store it, and by a read, which would
 * answer as though nothing held it — rather than read as a name nothing happens to use yet.
 */
export interface NameChecks {
  /** Refuses a relation kind, or any of a list of them, that isn't registered; `via` names the operation */
  relationKinds(kinds: string | readonly string[], via: string): void;
}

export function createNameChecks({ isRelationKind }: { isRelationKind: (name: string) => boolean }): NameChecks {
  return {
    relationKinds(kinds, via) {
      for (const kind of typeof kinds === 'string' ? [kinds] : kinds) {
        if (typeof kind === 'string' && isRelationKind(kind)) continue;
        throw new EarsError('UNKNOWN_RELATION_KIND', `${via}(${JSON.stringify(kind)}): not a registered relation kind`);
      }
    },
  };
}
