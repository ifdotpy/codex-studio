import { useServerApi } from "../../servers/ServerServicesContext";
import { Badge, Checkbox, Group, Text, VisuallyHidden } from "@mantine/core";
import { useId, useState } from "react";
import { accountDisplayName } from "../../accountName";
import { errorText } from "../../api";
import type { Agent } from "../../types";
import {
  AccountRemainingBar,
  ProviderMark,
  remainingText,
  useAccountLimitView,
} from "../AccountTiles";
import type { Account, AccountsState } from "../Accounts";

function EligibilityAccount({
  account,
  current,
  disabled,
}: {
  account: Account;
  current: boolean;
  disabled: boolean;
}) {
  const { remaining } = useAccountLimitView(account);
  const descriptionId = useId();
  const name = accountDisplayName(account);
  const fullName = [account.label, account.email].filter(Boolean).join(" — ");
  return (
    <div className="eligibility-account" title={fullName}>
      <Checkbox
        value={account.id}
        disabled={disabled}
        aria-label={name}
        aria-describedby={descriptionId}
        label={
          <span className="eligibility-label">
            <span className="eligibility-identity">
              <span className="eligibility-name">{name}</span>
              {account.email &&
                account.email !== account.label &&
                account.email !== name && (
                  <span className="eligibility-email">{account.email}</span>
                )}
            </span>
            {current && (
              <Badge className="eligibility-current" size="xs" variant="light">
                Current
              </Badge>
            )}
            <span className="eligibility-remaining">
              {remaining === null ? "Unavailable" : remainingText(remaining)}
            </span>
            <AccountRemainingBar
              remaining={remaining}
              label={`${name} remaining limit`}
            />
          </span>
        }
      />
      <VisuallyHidden id={descriptionId}>
        {fullName}
        {current ? "; Current account" : ""}
      </VisuallyHidden>
    </div>
  );
}

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
        <div className="execution-account-eligibility">
          {["codex", "claude"].map((provider) => {
            const grouped = accounts.accounts.filter(
              (account) => (account.provider || "codex") === provider,
            );
            return grouped.length ? (
              <section key={provider}>
                <Text
                  component="h4"
                  className="eligibility-provider"
                  size="xs"
                  c="dimmed"
                  fw={500}
                >
                  <ProviderMark provider={provider} />
                  {provider === "claude" ? "Claude" : "Codex"} {grouped.length}
                </Text>
                <div className="eligibility-provider-accounts">
                  {grouped.map((account) => (
                    <EligibilityAccount
                      key={account.id}
                      account={account}
                      current={account.id === lead.accountKey}
                      disabled={saving || account.id === lead.accountKey}
                    />
                  ))}
                </div>
              </section>
            ) : null;
          })}
        </div>
      </Checkbox.Group>
      <Text size="xs" c="dimmed" mt="sm">
        New accounts are enabled by default.
      </Text>
    </section>
  );
}
