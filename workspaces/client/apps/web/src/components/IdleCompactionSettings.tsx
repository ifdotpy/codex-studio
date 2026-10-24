import { NativeSelect, NumberInput } from "@mantine/core";
import { SettingsRow } from "./ui/primitives";
import {
  MAX_SAFE_INTEGER,
  IDLE_COMPACTION_DEFAULT_MIN_TOKENS,
  IDLE_COMPACTION_MIN_TOKENS,
} from "../generated/sharedValues";
import type { GetResult } from "../api";

type Settings = NonNullable<
  GetResult<"/api/orchestration/settings">["settings"]["idleCompaction"]
>;
export const idleCompactionDefaults: Settings = {
  mode: "off",
  minTokens: IDLE_COMPACTION_DEFAULT_MIN_TOKENS,
};

export function IdleCompactionSettings({
  value,
  disabled,
  onChange,
}: {
  value: Settings;
  disabled: boolean;
  onChange: (value: Settings) => void;
}) {
  return (
    <>
      <SettingsRow
        label="Claude idle compaction"
        help="Compact a long Claude conversation before its one-hour prompt cache expires."
      >
        <NativeSelect
          aria-label="Claude idle compaction"
          value={value.mode}
          disabled={disabled}
          data={[
            { value: "off", label: "Off" },
            { value: "log", label: "Log only" },
            { value: "compact", label: "Compact" },
          ]}
          onChange={(event) =>
            onChange({
              ...value,
              mode: event.currentTarget.value as Settings["mode"],
            })
          }
        />
      </SettingsRow>
      <SettingsRow
        label="Minimum context tokens"
        help={`The default is ${IDLE_COMPACTION_DEFAULT_MIN_TOKENS.toLocaleString("en-US")} tokens. The minimum is ${IDLE_COMPACTION_MIN_TOKENS.toLocaleString("en-US")} tokens. This setting applies to Claude agents.`}
      >
        <NumberInput
          aria-label="Idle compaction minimum context tokens"
          min={IDLE_COMPACTION_MIN_TOKENS}
          max={MAX_SAFE_INTEGER}
          aria-valuemin={IDLE_COMPACTION_MIN_TOKENS}
          allowDecimal={false}
          allowNegative={false}
          clampBehavior="strict"
          value={value.minTokens}
          disabled={disabled}
          onChange={(next) => {
            if (typeof next === "number")
              onChange({ ...value, minTokens: next });
          }}
        />
      </SettingsRow>
    </>
  );
}
