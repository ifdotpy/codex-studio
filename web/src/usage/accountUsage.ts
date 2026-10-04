import type { JsonValue } from "../types";
import type { GetResult } from "../api";
import type { components } from "../generated/api";
import { providerLimitData, providerRateLimitsDataDto } from "./providerLimits";

export type JsonObject = { [key: string]: JsonValue };

type AccountRateLimitsDto = components["schemas"]["AccountRateLimitsDto"];
type UsageLimitsResponse = GetResult<"/api/limits">;
type ClientLimitMetadata = { loading?: boolean; stale?: boolean };
export type AccountLimitsSnapshot =
  | (Pick<AccountRateLimitsDto, "accountKey" | "at" | "data" | "error"> &
      ClientLimitMetadata)
  | (Pick<
      UsageLimitsResponse,
      "accountKey" | "at" | "checkedAt" | "data" | "error" | "readAt"
    > &
      ClientLimitMetadata);

export function jsonObject(
  value: JsonValue | null | undefined,
): JsonObject | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value
    : null;
}

function isAccountLimitsSnapshot(
  value: JsonValue,
): value is AccountLimitsSnapshot {
  const limits = jsonObject(value);
  if (
    !limits ||
    typeof limits.accountKey !== "string" ||
    (limits.at !== undefined &&
      limits.at !== null &&
      (typeof limits.at !== "number" || !Number.isFinite(limits.at))) ||
    (limits.error !== undefined &&
      limits.error !== null &&
      typeof limits.error !== "string") ||
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
  value: JsonValue | null | undefined,
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
