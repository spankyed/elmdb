/**
 * Polls `check` until it stops throwing, and answers what it returned: `vi.waitFor` with no clock of its own.
 *
 * **The bound is the test's clock**, the minute `vitest.config.ts` gives a test or hook: a poll that never
 * settles fails there, and a deadline of its own would only add a guess about how long the work it waits for takes.
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
