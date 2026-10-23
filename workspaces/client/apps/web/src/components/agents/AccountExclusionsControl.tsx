import { useServerApi } from "../../servers/ServerServicesContext";
import { Checkbox, Group, Stack, Text } from "@mantine/core";
import { useState } from "react";
import { accountDisplayName } from "../../accountName";
import { errorText } from "../../api";
import type { Agent } from "../../types";
import { AccountRemainingText } from "../AccountTiles";
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
  const { post } = useServerApi();
  const [saving, setSaving] = useState(false);
  const enabled = accounts.accounts
    .filter((account) => !(lead.excludedAccountKeys || []).includes(account.id))
    .map((account) => account.id);
  if (!lead.isLead || !accounts.accounts.length) return null;
  return (
    <details className="execution-accounts">
      <summary>
        <span>Accounts</span>
        <Text component="span" size="xs" c="dimmed">
          {enabled.length} of {accounts.accounts.length} enabled
        </Text>
      </summary>
      <Checkbox.Group
        label="Accounts enabled for this chat"
        value={enabled}
        onChange={(value) => {
          setSaving(true);
          void post("/api/conversation", {
            id: lead.id,
            excluded_account_keys: accounts.accounts
              .filter((account) => !value.includes(account.id))
              .map((account) => account.id),
          })
            .then(refresh)
            .catch((cause) => onError(errorText(cause)))
            .finally(() => setSaving(false));
        }}
      >
        <Stack gap="sm" mt="xs">
          {accounts.accounts.map((account) => (
            <Checkbox
              key={account.id}
              value={account.id}
              label={accountDisplayName(account)}
              disabled={saving || account.id === lead.accountKey}
              description={
                <Group gap="xs">
                  <span>{account.email}</span>
                  <AccountRemainingText account={account} />
                  {account.id === lead.accountKey && (
                    <span>Current account</span>
                  )}
                </Group>
              }
            />
          ))}
        </Stack>
      </Checkbox.Group>
      <Text size="xs" c="dimmed" mt="sm">
        New accounts are enabled by default.
      </Text>
    </details>
  );
}
