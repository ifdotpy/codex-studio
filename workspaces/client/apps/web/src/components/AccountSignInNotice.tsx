import { organizationLabel } from "./ClaudeOrganization";
import { Button } from "@mantine/core";
import { errorText } from "../api";
import type { Account } from "./Accounts";

export default function AccountSignInNotice({
  account,
  errors,
  onSignIn,
}: {
  account?: Account;
  errors: unknown[];
  onSignIn: (accountKey: string) => void;
}) {
  if (!account) return null;
  const needsSignIn =
    !!account.organizationMismatch ||
    ["changed", "signedOut", "signed_out"].includes(account.status) ||
    [...errors, account.error, account.authenticationRecovery].some((error) =>
      /oauth|authentication|authenticate|unauthorized|refresh token|access token|not logged in|account changed|original login|sign in.*(claude|again)/i.test(
        errorText(error),
      ),
    );
  if (!needsSignIn) return null;
  const provider = account.provider === "claude" ? "Claude" : "Codex";
  return (
    <div className="sync-status">
      <span>
        {account.organizationMismatch
          ? `Signed in to a different organization: ${organizationLabel(account.organizationMismatch.observed)}. This profile requires ${organizationLabel(account.organizationMismatch.expected)}.`
          : `${provider} needs sign-in. ${account.email || account.label}`}
      </span>
      <Button size="compact-sm" onClick={() => onSignIn(account.id)}>
        Sign in to {provider}
      </Button>
    </div>
  );
}
