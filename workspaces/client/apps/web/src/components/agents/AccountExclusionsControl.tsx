import { Checkbox } from "@mantine/core";
import { useState } from "react";
import { accountDisplayName } from "../../accountName";
import { errorText, post } from "../../api";
import type { Agent } from "../../types";
import { ProviderMark } from "../AccountTiles";
import type { AccountsState } from "../Accounts";
import "./account-exclusions-control.css";

export function AccountExclusionsControl({
  lead,
  accounts,
  refresh,
  onError,
}: {
  lead: Agent;
  accounts: AccountsState;
  refresh: () => Promise<void>;
  onError: (message: string) => void;
}) {
  const [saving, setSaving] = useState(false);
  const excluded = new Set(lead.excludedAccountKeys || []);
  if (!lead.isLead || !accounts.accounts.length) return null;
  return (
    <fieldset className="account-exclusions-control" disabled={saving}>
      <legend>Accounts disabled for this chat</legend>
      {accounts.accounts.map((account) => (
        <Checkbox
          key={account.id}
          label={
            <span className="account-exclusions-option">
              <ProviderMark provider={account.provider || "codex"} />
              <span>
                <span>{accountDisplayName(account)}</span>
                {account.email && <small>{account.email}</small>}
              </span>
            </span>
          }
          checked={excluded.has(account.id)}
          onChange={(event) => {
            const next = new Set(excluded);
            if (event.currentTarget.checked) next.add(account.id);
            else next.delete(account.id);
            setSaving(true);
            void post("/api/conversation", {
              id: lead.id,
              excluded_account_keys: [...next],
            })
              .then(refresh)
              .catch((cause) => onError(errorText(cause)))
              .finally(() => setSaving(false));
          }}
        />
      ))}
      <small>New accounts are enabled by default.</small>
    </fieldset>
  );
}
