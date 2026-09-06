import { Button, Modal, NativeSelect, Switch } from "@mantine/core";
import { ChevronDown, Zap } from "lucide-react";
import { useState } from "react";
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

export function ExecutionControls({
  agent,
  catalog,
  refresh,
  onError,
}: {
  agent: Agent;
  catalog: Catalog;
  refresh: () => Promise<void>;
  onError: (text: string) => void;
}) {
  const [saving, setSaving] = useState(false);
  const info = infoFor(catalog, agent.model);
  const options = effortOptions(info);
  if (
    agent.effort &&
    !options.some((option) => option.value === agent.effort)
  ) {
    options.push({ value: agent.effort, label: title(agent.effort) });
  }
  const disabled =
    saving ||
    !!agent.inFlight ||
    busy.has(agent.status) ||
    catalog.loading ||
    !!catalog.error ||
    !info;
  const change = async (value: Json) => {
    setSaving(true);
    try {
      await api("/api/conversation", { id: agent.id, ...value });
      await refresh();
    } catch (error) {
      onError(errorText(error));
    } finally {
      setSaving(false);
    }
  };
  return (
    <div className="execution-controls">
      <NativeSelect
        className="execution-select execution-reasoning"
        aria-label={agent.isLead ? "Lead reasoning" : "Subagent reasoning"}
        title={
          disabled && (agent.inFlight || busy.has(agent.status))
            ? "Wait for this turn to end"
            : "Reasoning effort"
        }
        data={options}
        value={agent.effort || DEFAULT}
        disabled={disabled}
        onChange={(event) =>
          void change({
            effort:
              event.currentTarget.value === DEFAULT
                ? null
                : event.currentTarget.value,
          })
        }
      />
      <Button
        className="execution-fast"
        size="compact-xs"
        variant={agent.fastMode ? "light" : "subtle"}
        color={agent.fastMode ? "blue" : "gray"}
        leftSection={<Zap size={14} />}
        aria-label={agent.isLead ? "Lead fast mode" : "Subagent fast mode"}
        aria-pressed={!!agent.fastMode}
        title={
          fastTier(info)?.description ||
          "Fast mode is unavailable for this model"
        }
        disabled={disabled || (!agent.fastMode && !fastTier(info))}
        onClick={() => void change({ fast_mode: !agent.fastMode })}
      >
        Fast
      </Button>
    </div>
  );
}

export function WorkerDefaults({
  lead,
  catalog,
  refresh,
}: {
  lead: Agent;
  catalog: Catalog;
  refresh: () => Promise<void>;
}) {
  const [opened, setOpened] = useState(false);
  const [model, setModel] = useState(DEFAULT);
  const [effort, setEffort] = useState(DEFAULT);
  const [fast, setFast] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const selectedModel = model === DEFAULT ? lead.model : model;
  const info = infoFor(catalog, selectedModel);
  const options = effortOptions(info);
  const modelOptions = [
    { value: DEFAULT, label: `Same as lead (${shortModel(lead.model)})` },
    ...catalog.models.map((row) => ({
      value: row.model as string,
      label: row.displayName || row.model,
    })),
  ];
  if (!modelOptions.some((option) => option.value === model))
    modelOptions.push({ value: model, label: model });
  if (!options.some((option) => option.value === effort))
    options.push({ value: effort, label: title(effort) });
  const validEffort =
    effort === DEFAULT ||
    info?.supportedReasoningEfforts?.some(
      (row: Json) => row.reasoningEffort === effort,
    );
  const defaults = lead.workerDefaults;
  const summary = [
    shortModel(defaults?.model || lead.model),
    defaults?.effort || "default reasoning",
    defaults?.fastMode ? "Fast" : "Standard",
  ].join(" · ");
  return (
    <>
      <Button
        className="execution-menu worker-defaults-button"
        rightSection={<ChevronDown size={14} />}
        title={summary}
        aria-label="Subagent defaults"
        onClick={() => {
          setModel(defaults?.model || DEFAULT);
          setEffort(defaults?.effort || DEFAULT);
          setFast(!!defaults?.fastMode);
          setError("");
          setOpened(true);
        }}
      >
        Subagents · {shortModel(defaults?.model || lead.model)}
      </Button>
      <Modal
        opened={opened}
        onClose={() => !saving && setOpened(false)}
        title="Subagent defaults"
        size="sm"
      >
        <form
          className="worker-defaults-form"
          onSubmit={async (event) => {
            event.preventDefault();
            if (saving || !info || !validEffort || (fast && !fastTier(info)))
              return;
            setSaving(true);
            setError("");
            try {
              await api("/api/conversation", {
                id: lead.id,
                worker_defaults: {
                  model: model === DEFAULT ? null : model,
                  effort: effort === DEFAULT ? null : effort,
                  fast_mode: fast,
                },
              });
              await refresh();
              setOpened(false);
            } catch (failure) {
              setError(errorText(failure));
            } finally {
              setSaving(false);
            }
          }}
        >
          <p className="notice">
            For new subagents in this team. The orchestrator can override these
            settings for each launch.
          </p>
          <NativeSelect
            label="Default subagent model"
            data={modelOptions}
            value={model}
            disabled={saving || catalog.loading || !!catalog.error}
            onChange={(event) => {
              const next = event.currentTarget.value;
              const nextInfo = infoFor(
                catalog,
                next === DEFAULT ? lead.model : next,
              );
              setModel(next);
              if (
                !nextInfo?.supportedReasoningEfforts?.some(
                  (row: Json) => row.reasoningEffort === effort,
                )
              )
                setEffort(DEFAULT);
              if (!fastTier(nextInfo)) setFast(false);
            }}
          />
          <NativeSelect
            label="Default subagent reasoning"
            data={options}
            value={effort}
            disabled={saving || !info}
            onChange={(event) => setEffort(event.currentTarget.value)}
          />
          <Switch
            aria-label="Fast mode"
            label="Fast mode"
            description={
              fastTier(info)?.description || "Unavailable for this model"
            }
            checked={fast}
            disabled={saving || !fastTier(info)}
            onChange={(event) => setFast(event.currentTarget.checked)}
          />
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
          <div className="worker-defaults-footer">
            <Button onClick={() => setOpened(false)} disabled={saving}>
              Cancel
            </Button>
            <Button
              type="submit"
              variant="filled"
              loading={saving}
              disabled={
                catalog.loading ||
                !!catalog.error ||
                !info ||
                !validEffort ||
                (fast && !fastTier(info))
              }
            >
              Save defaults
            </Button>
          </div>
        </form>
      </Modal>
    </>
  );
}
