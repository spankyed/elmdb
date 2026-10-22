// Converts one environment written in storage format 1 to format 2, in place. Nothing in the app calls this: a
// format 1 data dir is refused at open (`checkFormat`), and the conversion is an explicit, one-off operation on a
// copy (`scripts/ears-format-2.ts`).
import * as fs from 'node:fs';
import * as path from 'node:path';
import { open, type Database } from 'lmdb';
import { entityTypeOf } from '../attribute-storage.ts';
import type { EARS } from '../entities.ts';
import { LMDB_FORMAT_VERSION } from './envs.ts';

const SEP = '\x1F';

/** What a conversion removed and rewrote, per kind of row */
export interface FormatConversion {
  /** Attribute rows of `entityType`: the id says it */
  entityTypeAttrs: number;
  /** Attribute rows of `relationDetails`: the relations table holds each relation */
  relationDetailAttrs: number;
  /** Entity rows of relations: a relation is its row in the relations table */
  relationRows: number;
  /** Entity rows no attribute names: written for a relation's ends, or left by a relation that was removed */
  orphanRows: number;
  /** Entity rows rewritten to hold only their type, which is the id's */
  rewrittenRows: number;
  /** Entities with attributes and no row, given one */
  addedRows: number;
}

/**
 * Converts the format 1 environment at `basePath` to format 2, in one transaction, and records the format.
 * Refuses an environment that records any format, since only one written before formats were recorded is
 * format 1.
 */
export function convertToFormat2(basePath: string): FormatConversion {
  if (!fs.existsSync(path.join(basePath, 'data.mdb'))) throw new Error(`No LMDB database at ${basePath}`);
  const root = open({ path: basePath, maxDbs: 8, compression: true });
  try {
    const meta = root.openDB({ name: 'meta', encoding: 'json' }) as Database<unknown>;
    const recorded = meta.get('format');
    if (recorded !== undefined && recorded !== 1) {
      throw new Error(`The database at ${basePath} records storage format ${JSON.stringify(recorded)}; only format 1 converts`);
    }
    const entities = root.openDB({ name: 'entities', encoding: 'json' }) as Database<Record<string, unknown>>;
    const attrs = root.openDB({ name: 'attrs', encoding: 'json' }) as Database<unknown>;

    const result: FormatConversion = { entityTypeAttrs: 0, relationDetailAttrs: 0, relationRows: 0, orphanRows: 0, rewrittenRows: 0, addedRows: 0 };
    root.transactionSync(() => {
      const withAttrs = new Set<string>();
      // Materialised, since the body removes keys from the range it walks
      for (const key of [...attrs.getKeys()] as string[]) {
        const kind = key.slice(0, key.indexOf(SEP));
        const id = key.slice(kind.length + 1, key.indexOf(SEP, kind.length + 1));
        if (kind === 'entityType') { attrs.remove(key); result.entityTypeAttrs++; }
        else if (kind === 'relationDetails') { attrs.remove(key); result.relationDetailAttrs++; }
        else withAttrs.add(id);
      }
      for (const { key, value } of [...entities.getRange()] as Array<{ key: string; value: Record<string, unknown> }>) {
        if (value?.type === 'Relation') { entities.remove(key); result.relationRows++; continue; }
        if (!withAttrs.has(key)) { entities.remove(key); result.orphanRows++; continue; }
        entities.put(key, { type: entityTypeOf(key as EARS.EntityId) });
        result.rewrittenRows++;
        withAttrs.delete(key);
      }
      for (const id of withAttrs) {
        entities.put(id, { type: entityTypeOf(id as EARS.EntityId) });
        result.addedRows++;
      }
      meta.put('format', LMDB_FORMAT_VERSION);
    });
    return result;
  } finally {
    root.close();
  }
}
