import { useEffect, useSyncExternalStore } from "react";
import { get } from "../api";
import { watchResourceReads } from "../components/watchResourceReads";
import {
  accountLimits,
  cachedAccountLimits,
  cacheAccountLimits,
  subscribeLimitsCache,
} from "./accountUsage";

export function useAccountLimits(
  accountKey: string,
  accountId?: string | null,
  enabled = true,
) {
  const snapshot = useSyncExternalStore(
    subscribeLimitsCache,
    () => cachedAccountLimits(accountKey, accountId),
    () => null,
  );
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    const stop = watchResourceReads(
      { kind: "limits", accountKey },
      async () => {
        const result = await get("/api/limits", {
          query: { account_key: accountKey },
          timeoutMs: 25000,
        });
        const next = accountLimits(result, accountKey, accountId);
        if (!next) throw new Error("Limits belong to another account.");
        if (active) cacheAccountLimits(next);
      },
      (error) => {
        if (!active) return;
        const previous = cachedAccountLimits(accountKey, accountId);
        const detail = error instanceof Error ? error.message : String(error);
        cacheAccountLimits(
          previous
            ? { ...previous, error: detail }
            : { accountKey, data: null, error: detail },
        );
      },
    );
    return () => {
      active = false;
      stop();
    };
  }, [accountKey, accountId, enabled]);
  return snapshot;
}
