import { useCallback, useMemo, useSyncExternalStore } from "react";
import { useServerResources } from "../../servers/ServerServicesContext";
import { defaultSubscriptions } from "./events";
import {
  subscriptionKey,
  type ResourceData,
  type ResourceRef,
  type SubscriptionOptions,
  type SubscriptionState,
} from "./contracts";

export type {
  ResourceData,
  ResourceRef,
  SubscriptionOptions,
  SubscriptionState,
  SubscriptionStatus,
} from "./contracts";
export { DEFAULT_SUBSCRIPTION_GRACE_MS } from "./contracts";
export { createSubscriptions } from "./module";

const inactive = { data: undefined, status: "stale", error: null } as const;

export function useSubscription<R extends ResourceRef>(
  ref: R | null,
  options: SubscriptionOptions = {},
): SubscriptionState<ResourceData<R>> {
  const resources = useServerResources();
  const subscriptions =
    "subscribe" in resources ? resources : defaultSubscriptions;
  const key = ref === null ? null : subscriptionKey(ref);
  // Caller object identity must never change its reference-counted interest.
  const stableRef = useMemo(() => ref, [key]);
  const subscribe = useCallback(
    (accept: () => void) =>
      stableRef === null
        ? () => {}
        : subscriptions.subscribe(stableRef, accept, {
            graceMs: options.graceMs,
          }),
    [subscriptions, stableRef, options.graceMs],
  );
  const snapshot = useCallback(
    () =>
      stableRef === null ? inactive : subscriptions.getSnapshot(stableRef),
    [subscriptions, stableRef],
  );
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}
