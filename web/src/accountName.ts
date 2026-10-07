import type { Account } from "./components/Accounts";

export function accountDisplayName(
  account: Pick<Account, "label" | "email">,
): string {
  const label = account.label.trim();
  // A stored label can be a full email; show only the part before "@".
  return label.split("@")[0] || account.email?.split("@")[0] || "Account";
}

export function accountTooltip(account: Pick<Account, "email">): string {
  return account.email || "";
}
