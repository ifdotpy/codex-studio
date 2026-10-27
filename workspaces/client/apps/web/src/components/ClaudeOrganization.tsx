import { ProviderKind } from "../generated/providerKind.ts";
import type { Account } from "./Accounts";

export function organizationLabel(
  organization: NonNullable<Account["organization"]>,
) {
  return `${organization.name || "Organization"} (${organization.idSuffix})`;
}

export default function ClaudeOrganization({
  account,
}: {
  account?: Account | undefined;
}) {
  if (account?.provider !== ProviderKind.Claude || !account.organization)
    return null;
  return (
    <small className="claude-organization">
      Organization: {organizationLabel(account.organization)}
    </small>
  );
}
