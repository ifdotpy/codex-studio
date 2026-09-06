import { Button, Popover, NativeSelect, Switch } from "@mantine/core";
import { ChevronDown } from "lucide-react";
import { useEffect, useState } from "react";
import { api, errorText } from "../api";
import { busy, type Agent, type Json } from "../types";
import type { useWorkerModels } from "./WorkerModelPicker";
import "./execution-settings.css";

type Catalog = ReturnType<typeof useWorkerModels>;
const DEFAULT = "__model_default__";
const title = (value: string) => value.charAt(0).toUpperCase() + value.slice(1);
const infoFor = (catalog: Catalog, model: string) =>
  catalog.models.find((row) => row.model === model);
const fastTier = (info?: Json) =>
  info?.serviceTiers?.find((tier: Json) => tier.id === "priority");
const effortOptions = (info?: Json) => [
  {
    value: DEFAULT,
    label: `Default${info?.defaultReasoningEffort ? ` (${info.defaultReasoningEffort})` : ""}`,
  },
  ...(info?.supportedReasoningEfforts || []).map((row: Json) => ({
    value: row.reasoningEffort,
    label: title(row.reasoningEffort),
  })),
];
export function shortModel(model: string) {
  return (
    (
      {
        "gpt-6-astra": "Astra",
        "gpt-5.6-sol": "Sol",
        "gpt-5.6-terra": "Terra",
        "gpt-5.6-luna": "Luna",
        "gpt-daybreak-blue-latest": "Daybreak Blue",
      } as Record<string, string>
    )[model] || model
  );
}

export function ExecutionSettings({
  agent,
  catalog,
  refresh,
  teamDefaults = false,
}: {
  agent: Agent;
  catalog: Catalog;
  refresh: () => Promise<void>;
  teamDefaults?: boolean;
}) {
  const [opened, setOpened] = useState(false);
  useEffect(() => {
    if (!opened) return;
    const close = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpened(false);
    };
    document.addEventListener("keydown", close);
    return () => document.removeEventListener("keydown", close);
  }, [opened]);
  const [saving, setSaving] = useState(false);
  const [pendingYolo, setPendingYolo] = useState<boolean | null>(null);
  const [pending, setPending] = useState<Json | null>(null);
  const [error, setError] = useState("");
  const label = teamDefaults
    ? "Subagent defaults"
    : agent.isLead
      ? "Lead settings"
      : "Subagent settings";
  const prefix = teamDefaults
    ? "Default subagent"
    : agent.isLead
      ? "Lead"
      : "Subagent";
  const stored = teamDefaults
    ? {
        model: agent.workerDefaults?.model ?? null,
        effort: agent.workerDefaults?.effort ?? null,
        fast_mode: !!agent.workerDefaults?.fastMode,
      }
    : {
        model: agent.model,
        effort: agent.effort ?? null,
        fast_mode: !!agent.fastMode,
      };
  const current = pending || stored;
  const selectedModel = current.model || agent.model;
  const info = infoFor(catalog, selectedModel);
  const active = !teamDefaults && (!!agent.inFlight || busy.has(agent.status));
  const disabled = saving || active || catalog.loading || !!catalog.error;
  const models = catalog.models.filter(
    (row) =>
      teamDefaults ||
      !agent.isLead ||
      ["gpt-6-astra", "gpt-5.6-sol"].includes(row.model),
  );
  const modelOptions = models.map((row) => ({
    value: row.model as string,
    label: row.displayName || shortModel(row.model),
  }));
  if (teamDefaults)
    modelOptions.unshift({
      value: DEFAULT,
      label: `Same as lead (${shortModel(agent.model)})`,
    });
  if (!modelOptions.some((row) => row.value === (current.model || DEFAULT)))
    modelOptions.unshift({
      value: current.model || DEFAULT,
      label: shortModel(selectedModel),
    });
  const options = effortOptions(info);
  if (current.effort && !options.some((row) => row.value === current.effort))
    options.push({ value: current.effort, label: title(current.effort) });
  const change = async (patch: Json) => {
    if (disabled) return;
    const next = { ...current, ...patch };
    if ("model" in patch) {
      const nextInfo = infoFor(catalog, next.model || agent.model);
      if (
        !nextInfo?.supportedReasoningEfforts?.some(
          (row: Json) => row.reasoningEffort === next.effort,
        )
      )
        next.effort = null;
      if (!fastTier(nextInfo)) next.fast_mode = false;
    }
    setPending(next);
    setSaving(true);
    setError("");
    try {
      await api("/api/conversation", {
        id: agent.id,
        ...(teamDefaults ? { worker_defaults: next } : next),
      });
      await refresh();
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      setPending(null);
      setSaving(false);
    }
  };
  const changeYolo = async (enabled: boolean) => {
    setPendingYolo(enabled);
    setSaving(true);
    setError("");
    try {
      await api("/api/conversation", { id: agent.id, yolo_mode: enabled });
      await refresh();
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      setPendingYolo(null);
      setSaving(false);
    }
  };
  return (
    <Popover
      opened={opened}
      onChange={setOpened}
      position="bottom-end"
      width={300}
      shadow="md"
      trapFocus
      returnFocus
    >
      <Popover.Target>
        <Button
          className="execution-menu"
          rightSection={<ChevronDown size={14} />}
          aria-label={label}
          aria-expanded={opened}
          title={[
            selectedModel,
            current.effort || "Default reasoning",
            current.fast_mode ? "Fast" : "Standard",
          ].join(" · ")}
          onClick={() => {
            setError("");
            setOpened(!opened);
          }}
        >
          {teamDefaults ? "Subagents" : agent.isLead ? "Lead" : "Subagent"} ·{" "}
          {shortModel(selectedModel)}
        </Button>
      </Popover.Target>
      <Popover.Dropdown
        className="execution-dropdown"
        role="dialog"
        aria-label={label}
      >
        <NativeSelect
          id={teamDefaults ? undefined : "model"}
          label={prefix + " model"}
          data={modelOptions}
          value={current.model || DEFAULT}
          disabled={disabled}
          onChange={(event) =>
            void change({
              model:
                event.currentTarget.value === DEFAULT
                  ? null
                  : event.currentTarget.value,
            })
          }
        />
        <NativeSelect
          label={prefix + " reasoning"}
          data={options}
          value={current.effort || DEFAULT}
          disabled={disabled || !info}
          onChange={(event) =>
            void change({
              effort:
                event.currentTarget.value === DEFAULT
                  ? null
                  : event.currentTarget.value,
            })
          }
        />
        <Switch
          label="Fast mode"
          aria-label="Fast mode"
          checked={current.fast_mode}
          disabled={disabled || (!current.fast_mode && !fastTier(info))}
          description={
            fastTier(info)?.description || "Unavailable for this model"
          }
          onChange={(event) =>
            void change({ fast_mode: event.currentTarget.checked })
          }
        />
        {!teamDefaults && agent.isLead && (
          <Switch
            label="YOLO mode"
            aria-label="YOLO mode"
            checked={pendingYolo ?? agent.yoloMode === true}
            disabled={saving || active || !("yoloMode" in agent)}
            description={
              !("yoloMode" in agent)
                ? "Available after the server update."
                : agent.yoloMode == null
                  ? "Uses the existing Codex permissions. Enable for full access without prompts."
                  : "Full access without permission prompts for the whole team. Account project rules still apply."
            }
            onChange={(event) => void changeYolo(event.currentTarget.checked)}
          />
        )}
        {teamDefaults && (
          <p className="notice">
            For new subagents. The orchestrator can override each launch.
          </p>
        )}
        {active && <p className="notice">Available when this turn ends.</p>}
        {catalog.error && (
          <div role="alert">
            <p>{catalog.error}</p>
            <Button onClick={catalog.retry}>Retry model list</Button>
          </div>
        )}
        {error && (
          <p className="execution-error" role="alert">
            {error}
          </p>
        )}
      </Popover.Dropdown>
    </Popover>
  );
}
