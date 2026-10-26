import { serverLocalStorage } from "../servers/storage";
import type { JsonInput, Json } from "../types";
import type { ApiError, GetResult } from "../api";
import type { components } from "../generated/api";
import { ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS } from "../generated/sharedValues";
import { providerLimitData, providerRateLimitsDataDto } from "./providerLimits";

export type JsonObject = Json;

type AccountRateLimitsDto = components["schemas"]["AccountRateLimitsDto"];
type UsageLimitsResponse = GetResult<"/api/limits">;
type ClientLimitMetadata = { loading?: boolean; stale?: boolean };
export type AccountLimitsReadError = Pick<ApiError, "message" | "status"> & {
  location?: string[];
};
export type AccountLimitsSnapshot =
  | (Pick<AccountRateLimitsDto, "accountKey" | "at" | "data" | "error"> &
      ClientLimitMetadata & { readError?: AccountLimitsReadError })
  | (Pick<
      UsageLimitsResponse,
      "accountKey" | "at" | "checkedAt" | "data" | "error" | "readAt"
    > &
      ClientLimitMetadata & { readError?: AccountLimitsReadError });

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

function isAccountLimitsSnapshot(
  value: JsonInput,
): value is AccountLimitsSnapshot {
  const limits = jsonObject(value);
  const readError = jsonObject(limits?.readError);
  if (
    !limits ||
    typeof limits.accountKey !== "string" ||
    (limits.at !== undefined &&
      limits.at !== null &&
      (typeof limits.at !== "number" || !Number.isFinite(limits.at))) ||
    (limits.error !== undefined &&
      limits.error !== null &&
      typeof limits.error !== "string") ||
    (limits.readError !== undefined &&
      (!readError ||
        typeof readError.message !== "string" ||
        typeof readError.status !== "number" ||
        !Number.isFinite(readError.status) ||
        (readError.location !== undefined &&
          (!Array.isArray(readError.location) ||
            !readError.location.every((part) => typeof part === "string"))))) ||
    (limits.readAt !== undefined &&
      limits.readAt !== null &&
      (typeof limits.readAt !== "number" || !Number.isFinite(limits.readAt))) ||
    (limits.checkedAt !== undefined &&
      limits.checkedAt !== null &&
      (typeof limits.checkedAt !== "number" ||
        !Number.isFinite(limits.checkedAt))) ||
    (limits.loading !== undefined && typeof limits.loading !== "boolean") ||
    (limits.stale !== undefined && typeof limits.stale !== "boolean") ||
    (limits.data !== undefined &&
      limits.data !== null &&
      !providerLimitData(limits.data) &&
      !providerRateLimitsDataDto(limits.data))
  )
    return false;
  return true;
}

export function accountLimits(
  value: JsonInput | null | undefined,
  accountKey: string,
  accountId?: string | null,
): AccountLimitsSnapshot | null {
  if (
    !value ||
    !isAccountLimitsSnapshot(value) ||
    value.accountKey !== accountKey
  )
    return null;
  const limits = value;
  const data = jsonObject(limits.data);
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
  value: JsonInput | null | undefined,
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
export function createAccountLimitsCache(
  storage: Pick<Storage, "getItem" | "setItem">,
) {
  let cacheScope: string | undefined;
  let cache: Record<string, AccountLimitsSnapshot> = {};
  const cacheListeners = new Set<() => void>();
  function configureLimitsCache(scope: string) {
    if (scope === cacheScope) return;
    cacheScope = scope;
    cache = {};
    try {
      const stored = JSON.parse(
        storage.getItem(`codex-limits:${scope}`) || "{}",
      );
      for (const [key, value] of Object.entries(stored)) {
        const snapshot = accountLimits(value as JsonInput, key);
        if (snapshot) cache[key] = snapshot;
      }
    } catch {
      /* Storage can be unavailable. */
    }
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
    try {
      storage.setItem(`codex-limits:${cacheScope}`, JSON.stringify(cache));
    } catch {
      /* Keep the session cache when storage is unavailable. */
    }
    for (const listener of cacheListeners) listener();
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
} = createAccountLimitsCache(serverLocalStorage);
