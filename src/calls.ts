// What a stretch of the app's work costs in crossings to elmdb: every read, every write made outside a unit, and every
// unit with the writes it held, each counted with the time it took and the code that made it. The connection records
// into it only while counting is started (`EarsAdmin.calls`), so an engine nobody is measuring pays one check per call.

/** What a counted call was: a read, a write outside a unit, or a unit, which holds the writes made inside it */
export type CallKind = 'read' | 'write' | 'unit';

/** The calls one piece of code made, of one kind */
export interface CallSite {
  /**
   * The code that made them: the first frames of the stack outside the engine, innermost first, joined by ` < `, each
   * a file and line. A source build names source files; a bundled one names the bundle, and `unknown` is a call no
   * frame outside the engine made
   */
  site: string;
  /** What the calls were */
  kind: CallKind;
  /** Whether the reads were a watch's run, made again after a commit: a view's re-run */
  watch: boolean;
  /** How many calls: reads, writes, or units */
  calls: number;
  /** The operations they carried: a read's plans, a write's ops, a unit's ops over all its writes */
  ops: number;
  /** For a unit, how many write calls it held */
  writes: number;
  /** Milliseconds spent in the calls, decoding their answers included; for a unit, its writes and its commit */
  ms: number;
}

/** One watch's runs while counting ran */
export interface WatchCount {
  /** The watch's label, or where it was opened */
  label: string;
  /** How many times it ran again after a commit, and the milliseconds those runs took */
  runs: number;
  ms: number;
  /** Commits that landed while it was already waiting to run: states its next run skipped */
  skipped: number;
}

/** Every call counted since counting started */
export interface CallReport {
  /** Whether counting is still running */
  counting: boolean;
  /** Milliseconds since counting started, to the report or to `stop` */
  elapsedMs: number;
  /** Each kind summed over every site */
  totals: Record<CallKind, { calls: number; ops: number; ms: number }> & {
    /** Watch runs: each a read made again after a commit that touched what it read */
    watchRuns: { calls: number; ms: number };
  };
  /** Every site, the most time first */
  sites: CallSite[];
  /** Every watch that ran or skipped a state, the most time first */
  watches: WatchCount[];
}

/** Counting an engine's calls: started, read while it runs, and stopped */
export interface CallCounting {
  /** Starts counting from nothing, dropping any count already held */
  start(): void;
  /** What has been counted since `start`; every total zero when counting never started */
  report(): CallReport;
  /** Stops counting and answers what it counted */
  stop(): CallReport;
}

/** How many frames outside the engine name a site: the caller, and the two callers that called it */
const SITE_FRAMES = 3;
/** How deep a stack is read for them, past the engine's own frames */
const STACK_DEPTH = 40;

function filePath(location: string): string {
  return location.startsWith('file://') ? decodeURIComponent(new URL(location).pathname) : location;
}

/** `file:line` of each frame in a V8 stack, the frame's location without its column */
function framesOf(stack: string): string[] {
  const frames: string[] = [];
  for (const line of stack.split('\n').slice(1)) {
    const match = /\(?([^\s()]+):(\d+):\d+\)?\s*$/.exec(line);
    if (match) frames.push(`${filePath(match[1]!)}:${match[2]}`);
  }
  return frames;
}

/** How a counted call, or a watch given no label, names the code that made it (`EARSRuntimeDeps.callSites`) */
export interface CallSiteNaming {
  /**
   * The engine's own frames, never a site, by their path. By default, a frame in the directory the engine's modules
   * were loaded from (its source or its build, `src/` or `dist/`). A program that bundles the engine into its own code
   * gives a pattern here: there that directory is the bundle's, holding the program's frames too, and the engine's
   * modules are named only through the bundle's source maps
   */
  engineFrames?: RegExp;
  /**
   * Directories a site names a path from, the last of them in the path on, which is where two machines' paths to one
   * file stop differing. `['/node_modules/']` by default; a path holding none of them is named whole
   */
  roots?: readonly string[];
}

/** The directory, with its separator, of the file a V8 frame (`file:line`) is in */
function directoryOf(frame: string): string {
  const file = frame.replace(/:\d+$/, '');
  return file.slice(0, Math.max(file.lastIndexOf('/'), file.lastIndexOf('\\')) + 1);
}

/** What names the code calling into an engine, as `CallSite.site` names it, for one engine's `naming` */
export function createCallSites({ engineFrames, roots = ['/node_modules/'] }: CallSiteNaming = {}): () => string {
  // This module's own frame says where the engine was loaded from: `import.meta` is empty in a CommonJS bundle
  const engineDirectory = engineFrames ? undefined : directoryOf(framesOf(new Error().stack ?? '')[0] ?? '');
  const isEngineFrame = (frame: string) => (engineFrames ? engineFrames.test(frame) : !!engineDirectory && frame.startsWith(engineDirectory));
  const shortened = (frame: string) => {
    const at = Math.max(...roots.map((dir) => frame.lastIndexOf(dir)));
    return at < 0 ? frame : frame.slice(at + 1);
  };
  return () => {
    const limit = Error.stackTraceLimit;
    Error.stackTraceLimit = STACK_DEPTH;
    const stack = new Error().stack ?? '';
    Error.stackTraceLimit = limit;
    const outside = framesOf(stack).filter((frame) => !isEngineFrame(frame) && !frame.startsWith('node:'));
    return outside.length ? outside.slice(0, SITE_FRAMES).map(shortened).join(' < ') : 'unknown';
  };
}

/** One count's tallies, and a unit being counted until it commits or aborts */
export interface CallTally {
  record(kind: CallKind, site: string, watch: boolean, ops: number, ms: number, writes?: number): void;
  watchRun(label: string, ms: number): void;
  watchSkipped(label: string): void;
  report(counting: boolean): CallReport;
}

export function createCallTally(): CallTally {
  const started = performance.now();
  const sites = new Map<string, CallSite>();
  const watches = new Map<string, WatchCount>();
  const watchOf = (label: string) => {
    let held = watches.get(label);
    if (!held) watches.set(label, held = { label, runs: 0, ms: 0, skipped: 0 });
    return held;
  };
  return {
    record(kind, site, watch, ops, ms, writes = 0) {
      const key = `${kind}\u0000${watch}\u0000${site}`;
      const held = sites.get(key);
      if (held) {
        held.calls += 1;
        held.ops += ops;
        held.writes += writes;
        held.ms += ms;
      } else sites.set(key, { site, kind, watch, calls: 1, ops, writes, ms });
    },
    watchRun(label, ms) {
      const held = watchOf(label);
      held.runs += 1;
      held.ms += ms;
    },
    watchSkipped(label) {
      watchOf(label).skipped += 1;
    },
    report(counting) {
      const totals = {
        read: { calls: 0, ops: 0, ms: 0 },
        write: { calls: 0, ops: 0, ms: 0 },
        unit: { calls: 0, ops: 0, ms: 0 },
        watchRuns: { calls: 0, ms: 0 },
      };
      for (const watch of watches.values()) {
        totals.watchRuns.calls += watch.runs;
        totals.watchRuns.ms += watch.ms;
      }
      for (const entry of sites.values()) {
        const total = totals[entry.kind];
        total.calls += entry.calls;
        total.ops += entry.ops;
        total.ms += entry.ms;
      }
      return {
        counting,
        elapsedMs: performance.now() - started,
        totals,
        sites: [...sites.values()].map((entry) => ({ ...entry })).sort((a, b) => b.ms - a.ms),
        watches: [...watches.values()].map((watch) => ({ ...watch })).sort((a, b) => b.ms - a.ms),
      };
    },
  };
}
