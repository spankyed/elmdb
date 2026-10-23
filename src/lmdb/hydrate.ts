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
  /** Where the counts go; `console.log` by default */
  log?: (message: string) => void;
}) {
  const { engine, env, log = console.log } = params;

  // Hydrate attributes
  let attrCount = 0;
  for (const { key, value } of env.attrs.getRange()) {
    // kind␟entityId: one row per attribute
    const keyStr = String(key);
    const sep = keyStr.indexOf(SEP);
    engine.bulkLoadAttr(keyStr.substring(sep + 1) as EARS.EntityId, keyStr.substring(0, sep) as EARS.AttrKind, dec(value));
    attrCount++;
  }

  // Hydrate relations
  let relCount = 0;
  for (const { key: relId, value: r } of env.relations.getRange()) {
    const relIdStr = String(relId);

    if (!r.src || typeof r.src !== 'string' || r.src.length === 0) {
      console.warn(`[Hydrate] Skipping relation with invalid src: relId=${relIdStr}, src=${r.src}`);
      continue;
    }
    if (!r.tgt || typeof r.tgt !== 'string' || r.tgt.length === 0) {
      console.warn(`[Hydrate] Skipping relation with invalid tgt: relId=${relIdStr}, tgt=${r.tgt}`);
      continue;
    }
    if (!r.kind || typeof r.kind !== 'string') {
      console.warn(`[Hydrate] Skipping relation with invalid kind: relId=${relIdStr}, kind=${r.kind}`);
      continue;
    }

    engine.bulkLoadAttr(
      relIdStr as EARS.EntityId,
      EARS.AttrKind.RelationDetails,
      {
        source: r.src,
        target: r.tgt,
        kind: r.kind,
        info: r.info ?? undefined,
      } satisfies EARS.RelationDetail
    );
    engine.addToIndex(relIdStr as EARS.EntityId, r.kind, r.src, r.tgt);
    relCount++;
  }

  log(`[LMDB] Hydrated ${attrCount} attributes, ${relCount} relations`);
}
