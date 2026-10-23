import { useServerApi } from "../../servers/ServerServicesContext";
import { Checkbox, Group, Text } from "@mantine/core";
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
    <section className="execution-accounts" aria-label="Account eligibility">
      <Checkbox.Group
        label={
          <Group gap="xs">
            <span>Accounts enabled for this chat</span>
            <Text component="span" size="xs" c="dimmed">
              {enabled.length} of {accounts.accounts.length} enabled
            </Text>
          </Group>
        }
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
        <div className="setup-accounts execution-account-eligibility">
          {accounts.accounts.map((account) => (
            <Checkbox
              className="setup-account"
              data-selected={enabled.includes(account.id) || undefined}
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
        </div>
      </Checkbox.Group>
      <Text size="xs" c="dimmed" mt="sm">
        New accounts are enabled by default.
      </Text>
    </section>
  );
}
