import {
  IdleCompactionSettings,
  idleCompactionDefaults,
} from "./IdleCompactionSettings";
import { BackgroundShellSettings } from "./BackgroundShellSettings";
import { useServerApi } from "../servers/ServerServicesContext";
import { useEffect, useState } from "react";
import { Button, NumberInput } from "@mantine/core";
import { errorText, type GetResult } from "../api";
import {
  CHILD_RESULT_COALESCE_MAX_SECONDS,
  CHILD_RESULT_COALESCE_MIN_SECONDS,
  IDLE_SESSION_UNLOAD_MAX_MINUTES,
  IDLE_SESSION_UNLOAD_MIN_MINUTES,
} from "../generated/sharedValues";
import { SettingsRow } from "./ui/primitives";

type Configuration = GetResult<"/api/orchestration/settings">;
type Settings = Configuration["settings"];

/** One whole-number orchestration setting and its generated bounds. */
export interface OrchestrationSettingSpec {
  field: Exclude<keyof Settings, "idleCompaction" | "shellPolicy">;
  label: string;
  help: string;
  ariaLabel: string;
  tooltip: string;
  min: number;
  max: number;
}

export const childResultHoldSpec: OrchestrationSettingSpec = {
  field: "childResultCoalesceSeconds",
  label: "Subagent result hold (seconds)",
  help: "Deliver results that finish close together in one lead turn.",
  ariaLabel: "Subagent result hold in seconds",
  tooltip:
    "When a lead has several subagents running, a finished subagent's result is held " +
    "for up to this many seconds so results that arrive close together are delivered " +
    "to the lead in one turn. The lead is woken immediately when all its subagents " +
    "have finished or when any other message arrives. 0 delivers every result immediately.",
  min: CHILD_RESULT_COALESCE_MIN_SECONDS,
  max: CHILD_RESULT_COALESCE_MAX_SECONDS,
};

export const idleSessionUnloadSpec: OrchestrationSettingSpec = {
  field: "idleSessionUnloadMinutes",
  label: "Unload idle sessions after (minutes)",
  help: "Free the memory of leads and chats that have nothing to do.",
  ariaLabel: "Unload idle sessions after minutes",
  tooltip:
    "A lead's or ordinary chat's Codex thread or Claude session without a running " +
    "turn, pending input, or active monitor is unloaded after this many minutes " +
    "and resumed automatically on the next message. An account's provider process " +
    "stops after this many minutes with no loaded thread. Workers spawned by a " +
    "lead unload as soon as their turn ends. 0 never unloads.",
  min: IDLE_SESSION_UNLOAD_MIN_MINUTES,
  max: IDLE_SESSION_UNLOAD_MAX_MINUTES,
};

const SPECS = [childResultHoldSpec, idleSessionUnloadSpec];

export function OrchestrationNumberSetting({
  spec,
  value,
  disabled,
  onChange,
}: {
  spec: OrchestrationSettingSpec;
  value: number;
  disabled: boolean;
  onChange: (value: number) => void;
}) {
  return (
    <SettingsRow label={spec.label} help={spec.help}>
      <NumberInput
        aria-label={spec.ariaLabel}
        title={spec.tooltip}
        min={spec.min}
        max={spec.max}
        allowDecimal={false}
        allowNegative={false}
        clampBehavior="strict"
        value={value}
        disabled={disabled}
        onChange={(next) => {
          if (typeof next === "number") onChange(next);
        }}
      />
    </SettingsRow>
  );
}

export function OrchestrationSettings({ active }: { active: boolean }) {
  const { get, post } = useServerApi();
  const [configuration, setConfiguration] = useState<Configuration | null>(
    null,
  );
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (!active) return;
    let cancelled = false;
    void get("/api/orchestration/settings").then(
      (value) => {
        if (!cancelled) {
          setConfiguration(value);
          setError("");
        }
      },
      (failure) => {
        if (!cancelled) setError(errorText(failure));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [active]);
  if (!active) return null;
  return (
    <section className="settings-group" aria-label="Agent orchestration">
      <h2>Agent orchestration</h2>
      {configuration && (
        <>
          {SPECS.map((spec) => (
            <OrchestrationNumberSetting
              key={spec.field}
              spec={spec}
              value={configuration.settings[spec.field]}
              disabled={busy}
              onChange={(value) =>
                setConfiguration({
                  ...configuration,
                  settings: { ...configuration.settings, [spec.field]: value },
                })
              }
            />
          ))}
          <BackgroundShellSettings
            configuration={configuration}
            setConfiguration={setConfiguration}
            disabled={busy}
          />
          <IdleCompactionSettings
            value={
              configuration.settings.idleCompaction ?? idleCompactionDefaults
            }
            disabled={busy}
            onChange={(value) =>
              setConfiguration({
                ...configuration,
                settings: { ...configuration.settings, idleCompaction: value },
              })
            }
          />
          <Button
            mt="md"
            loading={busy}
            onClick={() => {
              setBusy(true);
              setError("");
              void post("/api/orchestration/settings", configuration.settings)
                .then(setConfiguration, (failure) =>
                  setError(errorText(failure)),
                )
                .finally(() => setBusy(false));
            }}
          >
            Save orchestration settings
          </Button>
        </>
      )}
      {error && <p role="alert">{error}</p>}
    </section>
  );
}
