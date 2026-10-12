import { Checkbox, Stack, Text } from "@mantine/core";
import { useState } from "react";
import { accountDisplayName } from "../../accountName";
import { errorText, post } from "../../api";
import type { Agent } from "../../types";
import { ProviderMark } from "../AccountTiles";
import type { AccountsState } from "../Accounts";

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
    <Stack gap="xs">
      <Text fw={600} size="sm">
        Accounts enabled for this chat
      </Text>
      {accounts.accounts.map((account) => (
        <Checkbox
          key={account.id}
          disabled={saving || account.id === lead.accountKey}
          label={
            <span>
              <ProviderMark provider={account.provider || "codex"} />
              <span style={{ marginInlineStart: 8 }}>
                <span>{accountDisplayName(account)}</span>
                {account.email && <small>{account.email}</small>}
                {account.id === lead.accountKey && (
                  <small>Current account</small>
                )}
              </span>
            </span>
          }
          checked={!excluded.has(account.id)}
          onChange={(event) => {
            const next = new Set(excluded);
            if (event.currentTarget.checked) next.delete(account.id);
            else next.add(account.id);
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
      <Text size="xs" c="dimmed">
        New accounts are enabled by default.
      </Text>
    </Stack>
  );
}
