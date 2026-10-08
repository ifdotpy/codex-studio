import { Check } from "lucide-react";
import { SegmentedControl, Tooltip } from "@mantine/core";
import { ActionButton } from "../ui/primitives";
import type { Account } from "../Accounts";
import type { WorkerModelInfo } from "./WorkerModelPicker";
import { ProviderMark } from "../AccountTiles";
import { AccountTiles } from "../AccountTiles";
import { shortModel } from "./ExecutionSettings";

export type SetupRole = "orchestrator" | "worker" | "review";
export function setupModelName(info?: WorkerModelInfo, model = "") {
  // The provider logo already names the vendor, so drop "Claude" and "GPT".
  const id = info?.model || model;
  // gpt-6-astra -> "Astra 6", gpt-6.1-sol -> "Sol 6.1": name first, version last.
  const versioned = /^gpt-(\d[\d.]*)-([a-z][a-z-]*)$/i.exec(id);
  if (versioned)
    return `${versioned[2]
      .split("-")
      .map((word) => word[0].toUpperCase() + word.slice(1))
      .join(" ")} ${versioned[1]}`;
  const known = shortModel(id);
  if (known !== id) return known;
  return shortModel(info?.displayName || model)
    .replace(/^Claude\s*[·: ]\s*/i, "")
    .replace(/^Default \(recommended\)\s*[:·]?\s*/i, "")
    .replace(/^GPT[-\s]+/i, "")
    .replace(/^(\d[\d.]*)-(.+)$/, "$2 $1");
}
export type AgentSetupPickerProps = {
  role: SetupRole;
  onRole: (role: SetupRole) => void;
  roles: SetupRole[];
  provider: string;
  onProvider: (provider: string) => void;
  providers: string[];
  accounts: Account[];
  accountKey: string;
  onAccount: (key: string | null) => void;
  automaticAccount?: boolean;
  models: {
    value: string;
    label: string;
    isDefault?: boolean;
    disabled?: boolean;
  }[];
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
    <>
      <div className="setup-roles" role="group" aria-label="Agent role">
        {p.roles.map((role) => (
          <ActionButton
            key={role}
            aria-pressed={p.role === role}
            data-selected={p.role === role || undefined}
            onClick={() => p.onRole(role)}
          >
            {role === "orchestrator"
              ? "Orchestrator"
              : role === "worker"
                ? "Worker"
                : "Review"}
          </ActionButton>
        ))}
      </div>
      {p.role === "review" ? (
        <>
          <SegmentedControl
            fullWidth
            aria-label="Provider"
            value="codex"
            data={[
              {
                value: "codex",
                label: (
                  <span className="setup-provider">
                    <ProviderMark provider="codex" />
                    Codex
                  </span>
                ),
              },
            ]}
          />
          <p className="notice">Uses the caller account</p>
        </>
      ) : (
        <AccountTiles
          showLabel={false}
          label={
            p.role === "worker" ? "Subagent account" : "Main agent account"
          }
          accounts={p.accounts}
          value={p.accountKey}
          onChange={(value) => p.onAccount(value || null)}
          provider={p.provider}
          onProvider={p.onProvider}
          providers={p.providers}
          disabled={p.accountDisabled}
          automatic={p.automaticAccount}
        />
      )}
      <div
        id={p.role === "review" ? "review-model" : "model"}
        tabIndex={0}
        onKeyDown={(event) => {
          if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key))
            return;
          event.preventDefault();
          const options = Array.from(
            event.currentTarget.querySelectorAll<HTMLButtonElement>(
              "button:not(:disabled)",
            ),
          );
          const focused = options.indexOf(
            document.activeElement as HTMLButtonElement,
          );
          const index =
            event.key === "Home"
              ? 0
              : event.key === "End"
                ? options.length - 1
                : (focused +
                    (event.key === "ArrowDown" ? 1 : -1) +
                    options.length) %
                  options.length;
          options[index]?.focus();
        }}
        className="setup-models"
        role="listbox"
        aria-label={p.modelLabel}
        data-value={p.model}
      >
        {p.models.map((model) => (
          <ActionButton
            key={model.value}
            role="option"
            aria-selected={p.model === model.value}
            aria-disabled={p.disabled || model.disabled}
            data-value={model.value}
            data-inherited={model.value === "__model_default__" || undefined}
            data-selected={p.model === model.value || undefined}
            disabled={p.disabled || model.disabled}
            onClick={() => p.onModel(model.value)}
          >
            <span className="setup-model-name">{model.label}</span>
            {model.isDefault && <small>recommended</small>}
          </ActionButton>
        ))}
      </div>
      {p.efforts.length > 0 && (
        <SegmentedControl
          fullWidth
          className="setup-reasoning"
          size="xs"
          role="radiogroup"
          aria-label={p.effortLabel}
          data-value={p.effort}
          aria-disabled={p.disabled || p.effortDisabled}
          value={p.effort}
          onChange={p.onEffort}
          disabled={p.disabled || p.effortDisabled}
          data={p.efforts}
        />
      )}
      {p.role !== "review" && (p.fastAvailable || p.daybreakAvailable) && (
        <div className="setup-modes" role="group" aria-label="Optional modes">
          {p.fastAvailable && (
            <Tooltip label="Faster responses with increased usage.">
              <ActionButton
                leftSection={p.fast ? <Check size={14} /> : undefined}
                aria-label="Fast mode"
                aria-pressed={p.fast}
                data-selected={p.fast || undefined}
                disabled={p.disabled}
                onClick={p.onFast}
              >
                Fast mode
              </ActionButton>
            </Tooltip>
          )}
          {p.daybreakAvailable && (
            <Tooltip label="Use the selected model with Daybreak access.">
              <ActionButton
                leftSection={p.daybreak ? <Check size={14} /> : undefined}
                aria-label="Daybreak"
                aria-pressed={p.daybreak}
                data-selected={p.daybreak || undefined}
                disabled={p.disabled}
                onClick={p.onDaybreak}
              >
                Daybreak
              </ActionButton>
            </Tooltip>
          )}
        </div>
      )}
    </>
  );
}
