import type { Json } from "./types";

export function accountLimits(
  value: Json | null | undefined,
  accountKey: string,
  accountId?: string | null,
): Json | null {
  if (!value || value.accountKey !== accountKey) return null;
  if (accountId && value.data?.accountId && value.data.accountId !== accountId)
    return null;
  return value;
}
