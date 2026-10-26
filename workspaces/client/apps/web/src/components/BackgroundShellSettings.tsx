import { NumberInput, Switch } from "@mantine/core";
import type { GetResult } from "../api";
import {
  SHELL_QUIET_MIN_MS,
  SHELL_POLICY_DEFAULTS,
  MAX_SAFE_INTEGER,
  SHELL_PRESSURE_IDLE_MINUTES,
} from "../generated/sharedValues";
import { SettingsRow } from "./ui/primitives";

type Configuration = GetResult<"/api/orchestration/settings">;
type ShellPolicy = NonNullable<Configuration["settings"]["shellPolicy"]>;

export function BackgroundShellSettings({
  configuration,
  setConfiguration,
  disabled,
}: {
  configuration: Configuration;
  setConfiguration: (configuration: Configuration) => void;
  disabled: boolean;
}) {
  const value = configuration.settings.shellPolicy ?? SHELL_POLICY_DEFAULTS;
  const change = (policy: ShellPolicy) =>
    setConfiguration({
      ...configuration,
      settings: { ...configuration.settings, shellPolicy: policy },
    });
  return (
    <>
      <SettingsRow
        label="Background shell check-in (milliseconds)"
        help={`Check once after a shell has no new output. Use 0 to disable, or at least ${SHELL_QUIET_MIN_MS} milliseconds.`}
      >
        <NumberInput
          aria-label="Background shell quiet threshold in milliseconds"
          value={value.quietThresholdMs}
          min={0}
          max={MAX_SAFE_INTEGER}
          allowDecimal={false}
          allowNegative={false}
          disabled={disabled}
          error={
            value.quietThresholdMs > 0 &&
            value.quietThresholdMs < SHELL_QUIET_MIN_MS
              ? `Use 0 or at least ${SHELL_QUIET_MIN_MS} milliseconds.`
              : undefined
          }
          onChange={(next) => {
            if (typeof next === "number")
              change({ ...value, quietThresholdMs: next });
          }}
        />
      </SettingsRow>
      <SettingsRow
        label="Stop idle shells under critical memory pressure"
        help={`After ${SHELL_PRESSURE_IDLE_MINUTES} idle minutes, stop background shells when no turn or delegate is active. Studio records each stop and notifies the agent.`}
      >
        <Switch
          aria-label="Stop idle shells under critical memory pressure"
          checked={value.pressureCleanup}
          disabled={disabled}
          onChange={(event) =>
            change({ ...value, pressureCleanup: event.currentTarget.checked })
          }
        />
      </SettingsRow>
    </>
  );
}
