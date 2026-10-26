import type { NativeServerContext, NativeSubscriptionEvent } from "../client";

/** Own every opened view, including opens that settle after their UI closes. */
export function createNativeServiceLifecycle(
  context: Pick<NativeServerContext, "subscribe">,
  report: (error: unknown) => void,
) {
  const pending = new Set<Promise<unknown>>();
  const stops = new Set<() => void>();
  const failures: unknown[] = [];
  let disposed = false;
  let disposal: Promise<void> | undefined;
  const track = <T>(work: Promise<T>) => {
    pending.add(work);
    void work.then(
      () => pending.delete(work),
      (error) => {
        pending.delete(work);
        failures.push(error);
        report(error);
      },
    );
    return work;
  };
  return {
    watch<T>(
      topic: string,
      params: Record<string, unknown>,
      accept: (event: NativeSubscriptionEvent<T>) => void,
    ) {
      if (disposed) throw new Error("The native service is closed.");
      let active = true;
      let close: (() => Promise<void>) | undefined;
      const stop = () => {
        if (!active) return;
        active = false;
        stops.delete(stop);
        if (close) {
          const owned = close;
          close = undefined;
          track(owned());
        }
      };
      stops.add(stop);
      track(
        context
          .subscribe<T>(topic, params, (event) => {
            if (active && !disposed) accept(event);
          })
          .then((opened) => {
            if (active && !disposed) close = opened;
            else return track(opened());
          }),
      );
      return stop;
    },
    dispose() {
      if (disposal) return disposal;
      disposed = true;
      for (const stop of stops) stop();
      disposal = (async () => {
        while (pending.size) await Promise.allSettled(pending);
        if (failures.length)
          throw new AggregateError(
            failures,
            "A native service view did not close.",
          );
      })();
      return disposal;
    },
  };
}
