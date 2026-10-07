import type { Account } from "./components/Accounts";

export function accountDisplayName(
  account: Pick<Account, "label" | "email">,
): string {
  return account.label.trim() || account.email?.split("@")[0] || "Account";
}

export function accountTooltip(account: Pick<Account, "email">): string {
  return account.email || "";
}
