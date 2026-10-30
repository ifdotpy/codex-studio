// Readiness deadline for the prebuilt HTTP draft/entity fixture, in milliseconds.
// Vitest setup and the port notification wait consume the same owner.
export const DRAFT_SERVER_READY_TIMEOUT_MS = 30_000;

/** Resolve with the first value of an observable that satisfies the predicate. */
export function until<T>(
  source: { subscribe(next: (value: T) => void): { unsubscribe(): void } },
  predicate: (value: T) => boolean,
) {
  return new Promise<T>((resolve) => {
    const subscription = source.subscribe((value) => {
      if (!predicate(value)) return;
      queueMicrotask(() => subscription?.unsubscribe());
      resolve(value);
    });
  });
}
