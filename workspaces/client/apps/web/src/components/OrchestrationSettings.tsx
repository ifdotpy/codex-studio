import { useServerApi } from "../servers/ServerServicesContext";
import { useEffect, useState } from "react";
import { Button, NumberInput } from "@mantine/core";
import { errorText, type GetResult } from "../api";
import {
  CHILD_RESULT_COALESCE_MAX_SECONDS,
  CHILD_RESULT_COALESCE_MIN_SECONDS,
} from "../generated/sharedValues";
import { SettingsRow } from "./ui/primitives";

type Configuration = GetResult<"/api/orchestration/settings">;

export const childResultHoldTooltip =
  "When a lead has several subagents running, a finished subagent's result is held " +
  "for up to this many seconds so results that arrive close together are delivered " +
  "to the lead in one turn. The lead is woken immediately when all its subagents " +
  "have finished or when any other message arrives. 0 delivers every result immediately.";

export function ChildResultHoldSetting({
  seconds,
  disabled,
  onChange,
}: {
  seconds: number;
  disabled: boolean;
  onChange: (seconds: number) => void;
}) {
  return (
    <SettingsRow
      label="Subagent result hold (seconds)"
      help="Deliver results that finish close together in one lead turn."
    >
      <NumberInput
        aria-label="Subagent result hold in seconds"
        title={childResultHoldTooltip}
        min={CHILD_RESULT_COALESCE_MIN_SECONDS}
        max={CHILD_RESULT_COALESCE_MAX_SECONDS}
        allowDecimal={false}
        allowNegative={false}
        clampBehavior="strict"
        value={seconds}
        disabled={disabled}
        onChange={(value) => {
          if (typeof value === "number") onChange(value);
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
          <ChildResultHoldSetting
            seconds={configuration.settings.childResultCoalesceSeconds}
            disabled={busy}
            onChange={(childResultCoalesceSeconds) =>
              setConfiguration({
                ...configuration,
                settings: { childResultCoalesceSeconds },
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
