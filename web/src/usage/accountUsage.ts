import type { Json, JsonValue } from "../types";

export type JsonObject = { [key: string]: JsonValue };

export function jsonObject(
  value: JsonValue | null | undefined,
): JsonObject | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value
    : null;
}

export function accountLimits(
  value: JsonValue | null | undefined,
  accountKey: string,
  accountId?: string | null,
): Json | null {
  const limits = jsonObject(value);
  if (!limits || limits.accountKey !== accountKey) return null;
  const data = jsonObject(limits.data);
  if (accountId && data?.accountId != null && data.accountId !== accountId)
    return null;
  return limits;
}
