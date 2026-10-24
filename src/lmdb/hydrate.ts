import { EARS } from '../entities.ts';
import type { LmdbDbs } from './envs.ts';
import type { EarsAdmin } from '../engine.ts';

const SEP = '\x1F';

function dec(e: { t: string; v: unknown }): unknown {
  if (!e) return null;
  if (e.t === 'date' && typeof e.v === 'string') return new Date(e.v);
  return e.v;
}

/** Loads the environment's attributes and relations into `engine` */
export async function hydrate(params: {
  engine: EarsAdmin;
  env: LmdbDbs;
  /** Where the counts and the relations it skips go; `console.log` by default */
  log?: (message: string) => void;
}) {
  const { engine, env, log = console.log } = params;
  // One unit for the whole load: a commit per row is what a store pays for each, and a load is one act
  engine.bulkLoad(() => load(engine, env, log));
}

function load(engine: EarsAdmin, env: LmdbDbs, log: (message: string) => void) {
  // The entity table is the record of which entities exist: an attribute row is loaded only for one of them, since
  // a flush that fell back to row by row can have stored an entity's attributes and lost its row
  const entities = new Set<string>();
  for (const key of env.entities.getKeys()) entities.add(String(key));

  // Hydrate attributes
  let attrCount = 0;
  let orphaned = 0;
  for (const { key, value } of env.attrs.getRange()) {
    // kind␟entityId: one row per attribute
    const keyStr = String(key);
    const sep = keyStr.indexOf(SEP);
    const id = keyStr.substring(sep + 1);
    if (!entities.has(id)) { orphaned++; continue; }
    engine.bulkLoadAttr(id as EARS.EntityId, keyStr.substring(0, sep) as EARS.AttrKind, dec(value));
    attrCount++;
  }
  if (orphaned) log(`[Hydrate] Skipped ${orphaned} attribute rows of entities with no entity row`);

  // Hydrate relations
  let relCount = 0;
  for (const { key: relId, value: r } of env.relations.getRange()) {
    const relIdStr = String(relId);

    if (!r.src || typeof r.src !== 'string' || r.src.length === 0) {
      log(`[Hydrate] Skipping relation with invalid src: relId=${relIdStr}, src=${r.src}`);
      continue;
    }
    if (!r.tgt || typeof r.tgt !== 'string' || r.tgt.length === 0) {
      log(`[Hydrate] Skipping relation with invalid tgt: relId=${relIdStr}, tgt=${r.tgt}`);
      continue;
    }
    if (!r.kind || typeof r.kind !== 'string') {
      log(`[Hydrate] Skipping relation with invalid kind: relId=${relIdStr}, kind=${r.kind}`);
      continue;
    }

    engine.bulkLoadRelation(relIdStr as EARS.RelationId, r.kind, r.src, r.tgt, r.info ?? undefined);
    relCount++;
  }

  log(`[LMDB] Hydrated ${attrCount} attributes, ${relCount} relations`);
}
