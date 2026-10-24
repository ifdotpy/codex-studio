import { createSubscriptionStore, type SubscriptionTransport } from "./store";
import {
  subscriptionKey,
  type ResourceData,
  type ResourceRef,
  type SubscriptionOptions,
} from "./contracts";

export type ObserveSubscriptionData = <R extends ResourceRef>(
  ref: R,
  accept: (data: ResourceData<R>) => void,
  failed: (error: unknown) => void,
) => () => void;

/** A server owns one module instance. Peer aggregation stays in its transport. */
export function createSubscriptions<T extends SubscriptionTransport>(
  transport: T,
  observeData?: ObserveSubscriptionData,
) {
  const store = createSubscriptionStore(transport);
  const readers = new Map<string, { count: number; stop: () => void }>();
  return {
    ...transport,
    ...store,
    subscribe<R extends ResourceRef>(
      ref: R,
      accept: () => void,
      options?: SubscriptionOptions,
    ) {
      const stopObserve = store.observe(ref, accept);
      const stopInterest = store.watchResourceChanges(ref, () => {}, options);
      const key = subscriptionKey(ref);
      let reader = readers.get(key);
      if (!reader) {
        // Install ownership before attaching a synchronous cache source.
        reader = { count: 0, stop: () => {} };
        readers.set(key, reader);
        reader.stop =
          observeData?.(
            ref,
            (data) => store.publish(ref, data),
            (error) => store.fail(ref, error),
          ) ?? (() => {});
      }
      reader.count++;
      let stopped = false;
      return () => {
        if (stopped) return;
        stopped = true;
        stopObserve();
        stopInterest();
        if (--reader.count === 0) {
          reader.stop();
          readers.delete(key);
        }
      };
    },
    dispose() {
      for (const reader of readers.values()) reader.stop();
      readers.clear();
      store.dispose();
      if ("dispose" in transport && typeof transport.dispose === "function")
        transport.dispose();
    },
  };
}
