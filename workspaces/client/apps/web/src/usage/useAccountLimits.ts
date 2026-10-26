import { useServerAccountLimits } from "../servers/ServerServicesContext";
import { useSyncExternalStore } from "react";

/** Read the account snapshot projected from its streamed rateLimit entity. */
export function useAccountLimits(
  accountKey: string,
  accountId?: string | null,
  _enabled = true,
) {
  const { cachedAccountLimits, subscribeLimitsCache } =
    useServerAccountLimits();
  return useSyncExternalStore(
    subscribeLimitsCache,
    () => cachedAccountLimits(accountKey, accountId),
    () => null,
  );
}
