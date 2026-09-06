import { ActionIcon, NativeSelect } from "@mantine/core";
import { RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import { api, errorText } from "../api";
import { busy, type Agent, type Json } from "../types";

export function useWorkerModels(accountKey: string, enabled: boolean) {
  const [result, setResult] = useState<{
    key: string;
    models: Json[];
    error: string;
  } | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    if (!enabled) return;
    let active = true;
    setResult(null);
    api("/api/models?account_key=" + encodeURIComponent(accountKey))
      .then((data) => {
        if (active)
          setResult({ key: accountKey, models: data.data || [], error: "" });
      })
      .catch((error) => {
        if (active)
          setResult({ key: accountKey, models: [], error: errorText(error) });
      });
    return () => {
      active = false;
    };
  }, [accountKey, enabled, attempt]);
  const current = result?.key === accountKey ? result : null;
  return {
    models:
      current?.models.filter((model) => model.model && !model.hidden) || [],
    loading: !current,
    error: current?.error || "",
    retry: () => setAttempt((value) => value + 1),
  };
}

export default function WorkerModelPicker({
  agent,
  catalog,
  change,
  onError,
  id,
}: {
  agent: Agent;
  catalog: ReturnType<typeof useWorkerModels>;
  change: (id: string, model: string) => Promise<void>;
  onError: (error: string) => void;
  id?: string;
}) {
  const [saving, setSaving] = useState(false);
  const options = catalog.models.map((row) => ({
    value: row.model as string,
    label: row.displayName || row.model,
  }));
  if (!options.some((row) => row.value === agent.model)) {
    options.unshift({ value: agent.model, label: agent.model });
  }
  const active = busy.has(agent.status) || !!agent.inFlight;
  return (
    <div className="worker-model-picker">
      <NativeSelect
        className="execution-select"
        id={id}
        aria-label={`Model for ${agent.name}`}
        title={
          catalog.error ||
          agent.model +
            " · " +
            (active ? "Wait for this turn to end" : "Model for the next turn")
        }
        value={agent.model}
        data={options}
        disabled={
          active ||
          saving ||
          catalog.loading ||
          !!catalog.error ||
          !catalog.models.length
        }
        onChange={async (event) => {
          const model = event.currentTarget.value;
          setSaving(true);
          try {
            await change(agent.id, model);
          } catch (error) {
            onError(errorText(error));
          } finally {
            setSaving(false);
          }
        }}
      />
      {!!catalog.error && (
        <ActionIcon
          aria-label="Retry model list"
          title={catalog.error}
          onClick={catalog.retry}
        >
          <RefreshCw size={14} />
        </ActionIcon>
      )}
    </div>
  );
}
