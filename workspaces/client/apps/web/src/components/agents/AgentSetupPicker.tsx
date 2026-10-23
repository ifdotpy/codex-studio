import { SegmentedControl, Stack, Text } from "@mantine/core";
import type { Account } from "../Accounts";
import { AccountTiles } from "../AccountTiles";
import { ModelField } from "../models/ModelField";
import { EffortField } from "../models/EffortField";
import { ModeToggles } from "../models/ModeToggles";
import type { ModelOption } from "../models/modelOption";

export type SetupRole = "orchestrator" | "worker" | "review";
export type AgentSetupPickerProps = {
  role: SetupRole;
  onRole: (role: SetupRole) => void;
  roles: SetupRole[];
  provider: string;
  onProvider: (provider: string) => void;
  providers: string[];
  accounts: Account[];
  accountKey: string;
  accountLabel?: string;
  onAccount: (key: string | null) => void;
  automaticAccount?: boolean;
  isAccountDisabled?: (account: Account) => boolean;
  models: ModelOption[];
  model: string;
  onModel: (model: string) => void;
  modelLabel: string;
  efforts: { value: string; label: string }[];
  effort: string;
  onEffort: (effort: string) => void;
  effortLabel: string;
  disabled: boolean;
  effortDisabled?: boolean;
  accountDisabled?: boolean;
  fastAvailable: boolean;
  fast: boolean;
  onFast: () => void;
  daybreakAvailable: boolean;
  daybreak: boolean;
  onDaybreak: () => void;
};
export function AgentSetupPicker(p: AgentSetupPickerProps) {
  return (
    <Stack gap="sm" className="execution-profile">
      {p.roles.length > 0 && (
        <SegmentedControl
          fullWidth
          size="xs"
          role="group"
          aria-label="Agent role"
          value={p.role}
          onChange={(role) => p.onRole(role as SetupRole)}
          data={p.roles.map((value) => ({
            value,
            label:
              value === "orchestrator"
                ? "Orchestrator"
                : value === "worker"
                  ? "Worker"
                  : "Review",
          }))}
        />
      )}
      {p.role === "review" ? (
        <Text size="xs" c="dimmed">
          Uses the caller account.
        </Text>
      ) : (
        <AccountTiles
          showLabel={false}
          label={
            p.accountLabel ||
            (p.role === "worker" ? "Subagent account" : "Main agent account")
          }
          accounts={p.accounts}
          value={p.accountKey}
          onChange={(value) => p.onAccount(value || null)}
          provider={p.provider}
          onProvider={p.onProvider}
          providers={p.providers}
          disabled={p.accountDisabled}
          accountDisabled={p.isAccountDisabled}
          automatic={p.automaticAccount}
        />
      )}
      <ModelField
        id={p.role === "review" ? "review-model" : "model"}
        label={p.modelLabel}
        options={p.models}
        value={p.model}
        onChange={p.onModel}
        disabled={p.disabled}
      />
      <EffortField
        label={p.effortLabel}
        options={p.efforts}
        value={p.effort}
        onChange={p.onEffort}
        disabled={p.disabled || p.effortDisabled}
      />
      {p.role !== "review" && <ModeToggles {...p} />}
    </Stack>
  );
}
