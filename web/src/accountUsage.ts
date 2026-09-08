import type { Json } from "./types";

// An untagged legacy response belongs only to the default profile.
export function accountLimits(
  value: Json | null | undefined,
  accountKey: string,
  accountId?: string | null,
): Json | null {
  if (!value || (value.accountKey || "default") !== accountKey) return null;
  if (accountId && value.data?.accountId && value.data.accountId !== accountId)
    return null;
  return value;
}
