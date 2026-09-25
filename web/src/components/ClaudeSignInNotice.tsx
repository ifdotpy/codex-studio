import { Button } from "@mantine/core";
import { errorText } from "../api";
import type { Account } from "./Accounts";

export default function ClaudeSignInNotice({ account, errors, onSignIn }: {
  account?: Account;
  errors: unknown[];
  onSignIn: (accountKey: string) => void;
}) {
  if (account?.provider !== "claude") return null;
  const needsSignIn = ["changed", "signedOut", "signed_out"].includes(account.status) ||
    [...errors, account.error].some((error) =>
      /oauth|authentication|authenticate|not logged in|account changed|original login|sign in.*claude/i.test(errorText(error)),
    );
  if (!needsSignIn) return null;
  return (
    <div className="sync-status">
      <span>Claude needs sign-in.</span>
      <Button size="compact-sm" onClick={() => onSignIn(account.id)}>
        Sign in to Claude
      </Button>
    </div>
  );
}
