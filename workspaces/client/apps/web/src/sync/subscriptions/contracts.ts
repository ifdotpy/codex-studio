import type { components } from "../../generated/api";

export type ResourceRef = components["schemas"]["ResourceRef"];
/** The OpenAPI document owns this mapping; contracts:generate emits its types. */
export type ResourceData<R extends ResourceRef> =
  components["schemas"]["SubscriptionDataMap"][R["kind"]];

export type SubscriptionStatus = "loading" | "live" | "stale" | "error";
export type SubscriptionState<T> = {
  data: T | undefined;
  status: SubscriptionStatus;
  error: Error | null;
};
export type SubscriptionOptions = { graceMs?: number };
/** Retain interest after the final consumer leaves, in milliseconds. */
export const DEFAULT_SUBSCRIPTION_GRACE_MS = 20_000;
/** Maximum inactive domain snapshots retained by one server module, in entries. */
export const MAX_INACTIVE_SUBSCRIPTION_CACHE_ENTRIES = 128;

/** Stable identity for subscription ownership and durable cursor keys. Never decode it. */
export function subscriptionKey(ref: ResourceRef): string {
  return JSON.stringify(
    Object.entries(ref)
      .filter(([, value]) => value !== undefined)
      .sort(([left], [right]) => left.localeCompare(right)),
  );
}

export type UseSubscription = <R extends ResourceRef>(
  ref: R | null,
  options?: SubscriptionOptions,
) => SubscriptionState<ResourceData<R>>;
