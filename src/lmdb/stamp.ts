import type { Database, RootDatabase } from 'lmdb';

/** An environment's own records, beside its data: its kind and storage format */
const META_DB = 'meta';
const FORMAT_KEY = 'format';
const KIND_KEY = 'kind';

/** What an environment must be to open: its kind (none for an EARS database) and the format this version writes */
export interface Stamp {
  /** The `kind` it records; an EARS database records none, which is what its older files look like too */
  kind?: string;
  format: number;
  /** What it is called in a refusal: `database`, `record store` */
  noun: string;
  /** The format of files that record none but hold data: written before formats were recorded */
  unstamped?: (root: RootDatabase) => number | undefined;
  /** What to do with files in an older format, said in the refusal */
  older?: string;
}

const kindName = (kind: unknown) => (kind === undefined ? 'an EARS database' : `a ${JSON.stringify(kind)} store`);

/**
 * Checks the environment at `dir` is what `stamp` says, and stamps a new, empty one unless it's read-only. Throws
 * for another kind or format, naming both. A new environment is stamped before anything is written to it, and a
 * stamp that fails (a full disk) refuses the open: unstamped, this version's writes would read as another format
 * to the next open, while refusing an empty environment costs nothing.
 */
export function checkStamp(root: RootDatabase, dir: string, readOnly: boolean, stamp: Stamp): void {
  // A read-only environment has no database the writer never created, and old files have none either
  const meta = root.openDB({ name: META_DB, encoding: 'json' }) as Database<unknown> | undefined;
  let format = meta?.get(FORMAT_KEY);
  if (format === undefined) format = stamp.unstamped?.(root);
  if (format === undefined) {
    if (readOnly || !meta) return;
    try {
      if (stamp.kind !== undefined) meta.putSync(KIND_KEY, stamp.kind);
      meta.putSync(FORMAT_KEY, stamp.format);
    } catch (error) {
      throw new Error(`Couldn't record the storage format of ${dir}, so it isn't opened: ${(error as Error).message}`);
    }
    return;
  }
  const kind = meta?.get(KIND_KEY);
  if (kind !== stamp.kind) throw new Error(`The files at ${dir} are ${kindName(kind)}, not ${stamp.kind === undefined ? 'an EARS database' : `a ${stamp.noun}`}`);
  if (format !== stamp.format) {
    const newer = typeof format === 'number' && format > stamp.format;
    const older = typeof format === 'number' && format < stamp.format;
    throw new Error(
      `The ${stamp.noun} at ${dir} is in storage format ${JSON.stringify(format)}, but this version reads format ${stamp.format}`
      + (newer ? ': it was written by a newer apack, so update this one' : older && stamp.older ? `: ${stamp.older}` : ''),
    );
  }
}
