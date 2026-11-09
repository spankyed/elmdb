/**
 * Polls `check` until it stops throwing, and answers what it returned: `vi.waitFor` with no clock of its own.
 *
 * **A local copy on purpose, and the only honest place for it.** The app's is `settles`
 * (`@apackdev/sdk/testing`), one layer up, and this package depends on no `@apackdev` package at all — which
 * is what lets the engine be developed, benchmarked and published on its own beside its Rust checkout. A spec
 * reaching into `apack-sdk/src` for it is refused by `spec-placement`, and rightly: that import is what would
 * end the standing alone. So these eight lines are the price of staying extractable. The two are one rule and
 * need not share code; neither is the other's fallback.
 *
 * **The bound is the progress guard this package's config already loads** (`vitest-progress-guard.ts`, beside
 * `testTimeout: 0`): it fails a test whose process tree stops working, or spends its CPU budget. So a poll
 * here is bounded exactly as the app's is, and a deadline on the clock would only add a guess about duration
 * — one that fails correct code on a loaded machine while the work it is waiting for is still progressing.
 */
export function settles<T>(check: () => T | Promise<T>, { interval = 50 }: { interval?: number } = {}): Promise<T> {
  return new Promise<T>((resolve) => {
    const attempt = async (): Promise<void> => {
      try {
        resolve(await check());
      } catch {
        setTimeout(() => { void attempt(); }, interval).unref();
      }
    };
    void attempt();
  });
}
