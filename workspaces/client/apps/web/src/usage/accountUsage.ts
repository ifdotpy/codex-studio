import type { JsonInput, JsonInputOf, Json } from "../types";
import type { ApiError, GetResult } from "../api";
import { ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS } from "../generated/sharedValues";
import { nativeUsage, type UsagePersistence } from "./usageRequest";

export type JsonObject = Json;

type UsageLimitsResponse = JsonInputOf<GetResult<"/api/limits">>;
type ClientLimitMetadata = { loading?: boolean; stale?: boolean };
export type AccountLimitsReadError = Pick<ApiError, "message" | "status"> & {
  location?: string[];
};
export type AccountLimitsSnapshot = UsageLimitsResponse &
  ClientLimitMetadata & { readError?: AccountLimitsReadError };

export function limitsReadSucceeded(
  snapshot: Pick<AccountLimitsSnapshot, "error">,
): boolean {
  return !snapshot.error;
}

export function limitsHydrationSucceeded(
  snapshot: Pick<AccountLimitsSnapshot, "data" | "error"> | null,
): boolean {
  return !!snapshot?.data && limitsReadSucceeded(snapshot);
}

export function jsonObject(
  value: JsonInput | null | undefined,
): JsonObject | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value
    : null;
}

export function accountLimits(
  value: AccountLimitsSnapshot | null | undefined,
  accountKey: string,
  accountId?: string | null,
): AccountLimitsSnapshot | null {
  if (!value || value.accountKey !== accountKey) return null;
  const limits = value;
  const data = limits.data;
  if (accountId && data?.accountId != null && data.accountId !== accountId)
    return null;
  return limits;
}

export function shouldReplaceLimitsSnapshot(
  current: AccountLimitsSnapshot | null | undefined,
  next: AccountLimitsSnapshot,
): boolean {
  if (next.data === null && limitsReadSucceeded(next)) return true;
  return !current || (current.at || 0) <= (next.at || 0);
}

export function limitsSnapshotIsFresh(
  value: AccountLimitsSnapshot | null | undefined,
  accountKey: string,
  accountId?: string | null,
  now = Date.now() / 1000,
): boolean {
  const snapshot = accountLimits(value, accountKey, accountId);
  return !!(
    snapshot &&
    limitsReadSucceeded(snapshot) &&
    typeof snapshot.at === "number" &&
    Number.isFinite(snapshot.at) &&
    now - snapshot.at < ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS
  );
}

// Each server owns its in-memory snapshots, listeners, and durable namespace.
// Memory is the synchronous read model. Accepted writes go to the native core in order.
export function createAccountLimitsCache(
  usage: UsagePersistence = nativeUsage,
) {
  let cacheScope: string | undefined;
  let cache: Record<string, AccountLimitsSnapshot> = {};
  const cacheListeners = new Set<() => void>();
  let writes: Promise<unknown> = Promise.resolve();
  function notifyListeners() {
    for (const listener of cacheListeners) listener();
  }
  function configureLimitsCache(scope: string) {
    if (scope === cacheScope) return;
    cacheScope = scope;
    cache = {};
    const request = usage();
    if (!request) return;
    request<Record<string, JsonInput>>("usage.limits.list", { scope }).then(
      (stored) => {
        if (scope !== cacheScope) return;
        let changed = false;
        for (const [key, value] of Object.entries(stored)) {
          // The core returns only what cacheAccountLimits wrote for this scope.
          const snapshot = accountLimits(value as AccountLimitsSnapshot, key);
          if (snapshot && shouldReplaceLimitsSnapshot(cache[key], snapshot)) {
            cache[key] = snapshot;
            changed = true;
          }
        }
        if (changed) notifyListeners();
      },
      () => {
        /* Keep live values when the native core is unavailable. */
      },
    );
  }
  function cachedAccountLimits(key: string, accountId?: string | null) {
    return accountLimits(cache[key], key, accountId);
  }
  function subscribeLimitsCache(listener: () => void) {
    cacheListeners.add(listener);
    return () => {
      cacheListeners.delete(listener);
    };
  }
  function cacheAccountLimits(snapshot: AccountLimitsSnapshot) {
    if (cacheScope === undefined) configureLimitsCache("");
    const previous = cache[snapshot.accountKey];
    if (previous === snapshot) return;
    if (!shouldReplaceLimitsSnapshot(previous, snapshot)) return;
    cache = { ...cache, [snapshot.accountKey]: snapshot };
    const scope = cacheScope;
    const request = usage();
    if (request)
      writes = writes
        .then(() => request("usage.limits.put", { scope, snapshot }))
        .catch(() => {
          /* Keep the session cache when the native core is unavailable. */
        });
    notifyListeners();
  }
  function cachedLimitsByAccount() {
    return cache;
  }

  return {
    configureLimitsCache,
    cachedAccountLimits,
    subscribeLimitsCache,
    cacheAccountLimits,
    cachedLimitsByAccount,
  };
}

export const {
  configureLimitsCache,
  cachedAccountLimits,
  subscribeLimitsCache,
  cacheAccountLimits,
  cachedLimitsByAccount,
} = createAccountLimitsCache();
