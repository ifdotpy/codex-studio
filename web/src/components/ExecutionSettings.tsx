import { Button, Popover, NativeSelect, Switch } from "@mantine/core";
import { ChevronDown } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, ApiError, errorText, save, saved } from "../api";
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

type SettingsProps = {
  agent: Agent;
  catalog: Catalog;
  refresh: () => Promise<void>;
  teamDefaults?: boolean;
  nextTurnSupported?: boolean;
  onOpenChange?: (opened: boolean) => void;
  openRequest?: number;
};
const accountOf = (agent: Agent) => agent.accountKey || "default";
const settingsFor = (agent: Agent, teamDefaults: boolean) => {
  if (teamDefaults)
    return {
      model:
        agent.workerDefaults?.model === undefined
          ? "gpt-5.6-luna"
          : agent.workerDefaults.model,
      effort:
        agent.workerDefaults?.effort === undefined
          ? "max"
          : agent.workerDefaults.effort,
      fast_mode: !!agent.workerDefaults?.fastMode,
    };
  const queued =
    agent.pendingSettingsAccountKey &&
    agent.pendingSettingsAccountKey !== accountOf(agent)
      ? null
      : agent.pendingSettings;
  return {
    model: queued?.model ?? agent.model,
    effort: queued ? (queued.effort ?? null) : (agent.effort ?? null),
    fast_mode: queued ? !!queued.fastMode : !!agent.fastMode,
  };
};
const sameSettings = (left: Json, right: Json) =>
  left.model === right.model &&
  left.effort === right.effort &&
  left.fast_mode === right.fast_mode;

export function ExecutionSettings(props: SettingsProps) {
  // Account transfers reuse the same agent ID in the conversation view.
  const scope = JSON.stringify([
    props.agent.id,
    accountOf(props.agent),
    !!props.teamDefaults,
  ]);
  return <ScopedExecutionSettings key={scope} {...props} />;
}

function ScopedExecutionSettings({
  agent,
  catalog,
  refresh,
  teamDefaults = false,
  nextTurnSupported,
  onOpenChange,
  openRequest = 0,
}: SettingsProps) {
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const canQueueSettings =
    nextTurnSupported ??
    (agent as Agent & { nextTurnSettingsSupported?: boolean })
      .nextTurnSettingsSupported === true;
  const [opened, setOpened] = useState(false);
  useEffect(() => {
    if (openRequest > 0) setOpened(true);
  }, [openRequest]);
  useEffect(() => {
    onOpenChange?.(opened);
    return () => onOpenChange?.(false);
  }, [opened, onOpenChange]);
  useEffect(() => {
    if (!opened) return;
    const close = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        setOpened(false);
      }
    };
    document.addEventListener("keydown", close);
    return () => document.removeEventListener("keydown", close);
  }, [opened]);
  const [saving, setSaving] = useState(false);
  const saveLock = useRef(false);
  const legacyReceiptKey = `next-turn-settings:${agent.id}`;
  const receiptKey = `next-turn-settings:${JSON.stringify([agent.id, accountOf(agent)])}`;
  const [legacyReceipt, setLegacyReceipt] = useState(
    () => !teamDefaults && !!saved<Json | null>(legacyReceiptKey, null),
  );
  const [unconfirmed, setUnconfirmed] = useState<Json | null>(() => {
    if (teamDefaults) return null;
    const value = saved<Json | null>(receiptKey, null);
    return value?.id === agent.id && value?.next_turn === true ? value : null;
  });
  const [pendingYolo, setPendingYolo] = useState<{
    value: boolean;
    baseline: boolean;
  } | null>(null);
  useEffect(() => {
    if (
      !saving &&
      pendingYolo &&
      (pendingYolo.value === (agent.yoloMode === true) ||
        pendingYolo.baseline !== (agent.yoloMode === true))
    )
      setPendingYolo(null);
  }, [saving, pendingYolo, agent.yoloMode]);
  const [pending, setPending] = useState<{
    values: Json;
    baseline: Json;
  } | null>(null);
  const [error, setError] = useState("");
  const [adjustment, setAdjustment] = useState("");
  const label = teamDefaults
    ? "Subagent defaults"
    : agent.isLead
      ? "Main agent settings"
      : "Subagent settings";
  const prefix = teamDefaults
    ? "Default subagent"
    : agent.isLead
      ? "Main agent"
      : "Subagent";
  const queued = (
    agent as Agent & {
      pendingSettings?: {
        model?: string;
        effort?: string | null;
        fastMode?: boolean;
      };
    }
  ).pendingSettings;
  const stored = settingsFor(agent, teamDefaults);
  const current = unconfirmed || pending?.values || stored;
  useEffect(() => {
    // Keep the acknowledged response while replication still has the old values.
    // A settings change in the snapshot hands ownership back to replication.
    if (
      !saving &&
      pending &&
      (sameSettings(pending.values, stored) ||
        !sameSettings(pending.baseline, stored))
    )
      setPending(null);
  }, [saving, pending, stored.model, stored.effort, stored.fast_mode]);
  const selectedModel = current.model || agent.model;
  const info = infoFor(catalog, selectedModel);
  const active = !teamDefaults && (!!agent.inFlight || busy.has(agent.status));
  const disabled =
    saving ||
    !!unconfirmed ||
    (active && !canQueueSettings) ||
    catalog.loading ||
    !!catalog.error;
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
      label: `Same as main agent (${shortModel(agent.model)})`,
    });
  if (!modelOptions.some((row) => row.value === (current.model || DEFAULT)))
    modelOptions.unshift({
      value: current.model || DEFAULT,
      label: shortModel(selectedModel),
    });
  const options = effortOptions(info);
  if (current.effort && !options.some((row) => row.value === current.effort))
    options.push({ value: current.effort, label: title(current.effort) });
  const submit = async (request: Json, next: Json, notice = "") => {
    if (saveLock.current) return;
    saveLock.current = true;
    request = { ...request, expected_account_key: accountOf(agent) };
    const previousPending = unconfirmed ? null : pending;
    setPending({ values: next, baseline: stored });
    setSaving(true);
    setError("");
    if (request.next_turn) {
      save(receiptKey, request);
      setUnconfirmed(request);
    }
    const clearReceipt = () => {
      if (
        saved<Json | null>(receiptKey, null)?.request_id === request.request_id
      )
        save(receiptKey, null);
    };
    let confirmed = false;
    try {
      const canonical = await api<Agent>(
        "/api/conversation",
        request,
        request.next_turn ? { timeoutMs: 15000 } : {},
      );
      if (
        !canonical ||
        canonical.id !== agent.id ||
        accountOf(canonical) !== accountOf(agent)
      )
        throw new Error(
          "The settings account changed. Check the current settings.",
        );
      confirmed = true;
      if (request.next_turn) {
        clearReceipt();
        if (mounted.current) setUnconfirmed(null);
      }
      if (!mounted.current) return;
      setPending({
        values: settingsFor(canonical, teamDefaults),
        baseline: stored,
      });
      setAdjustment(notice);
      await refresh();
    } catch (failure) {
      const rejected =
        failure instanceof ApiError &&
        [400, 401, 403, 404, 409, 422].includes(failure.status);
      if (!confirmed && (!request.next_turn || rejected)) {
        if (mounted.current) setPending(previousPending);
        if (request.next_turn) {
          clearReceipt();
          if (mounted.current) setUnconfirmed(null);
        }
      }
      if (!mounted.current) return;
      setError(
        confirmed
          ? `Settings saved. ${errorText(failure)}`
          : errorText(failure),
      );
    } finally {
      saveLock.current = false;
      if (mounted.current) setSaving(false);
    }
  };
  const change = async (patch: Json) => {
    if (disabled) return;
    const next = { ...current, ...patch };
    const adjustments: string[] = [];
    if ("model" in patch) {
      const nextInfo = infoFor(catalog, next.model || agent.model);
      if (
        !nextInfo?.supportedReasoningEfforts?.some(
          (row: Json) => row.reasoningEffort === next.effort,
        )
      ) {
        if (next.effort)
          adjustments.push("Reasoning changed to the model default.");
        next.effort = null;
      }
      if (!fastTier(nextInfo)) {
        if (next.fast_mode)
          adjustments.push("Fast mode is unavailable and was turned off.");
        next.fast_mode = false;
      }
    }
    const request = {
      id: agent.id,
      ...(teamDefaults
        ? { worker_defaults: next }
        : {
            ...next,
            ...(active || queued
              ? { next_turn: true, request_id: crypto.randomUUID() }
              : {}),
          }),
    };
    await submit(request, next, adjustments.join(" "));
  };
  const changeYolo = async (enabled: boolean) => {
    if (saveLock.current) return;
    saveLock.current = true;
    const previousPending = pendingYolo;
    setPendingYolo({ value: enabled, baseline: agent.yoloMode === true });
    setSaving(true);
    setError("");
    let confirmed = false;
    try {
      const canonical = await api<Agent>("/api/conversation", {
        id: agent.id,
        yolo_mode: enabled,
        expected_account_key: accountOf(agent),
      });
      if (
        !canonical ||
        canonical.id !== agent.id ||
        accountOf(canonical) !== accountOf(agent)
      )
        throw new Error(
          "The settings account changed. Check the current settings.",
        );
      confirmed = true;
      if (!mounted.current) return;
      setPendingYolo({
        value: canonical.yoloMode === true,
        baseline: agent.yoloMode === true,
      });
      await refresh();
    } catch (failure) {
      if (!mounted.current) return;
      if (!confirmed) setPendingYolo(previousPending);
      setError(
        confirmed
          ? `Settings saved. ${errorText(failure)}`
          : errorText(failure),
      );
    } finally {
      saveLock.current = false;
      if (mounted.current) setSaving(false);
    }
  };
  return (
    <Popover
      opened={opened}
      onChange={setOpened}
      position={openRequest > 0 ? "top-start" : "bottom-end"}
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
          {teamDefaults
            ? "Subagents"
            : agent.isLead
              ? "Main agent"
              : "Subagent"}{" "}
          · {shortModel(selectedModel)}
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
          <details className="execution-permissions">
            <summary>Permissions</summary>
            <p>
              These permissions apply to the main agent and every subagent in
              this team.
            </p>
            <Switch
              label="Full access without approval"
              aria-label="Full access without approval"
              checked={pendingYolo?.value ?? agent.yoloMode === true}
              disabled={saving || active || !("yoloMode" in agent)}
              description={
                !("yoloMode" in agent)
                  ? "Available after the server update."
                  : agent.yoloMode == null
                    ? "The team currently uses the existing Codex permissions."
                    : "When enabled, the team can use tools without permission prompts."
              }
              onChange={(event) => void changeYolo(event.currentTarget.checked)}
            />
          </details>
        )}
        {adjustment && (
          <p role="status" className="notice">
            {adjustment}
          </p>
        )}
        {teamDefaults && (
          <p className="notice">
            For new subagents. The main agent can change these settings when it
            starts a subagent.
          </p>
        )}
        {((!teamDefaults && queued) || (active && canQueueSettings)) && (
          <p className="notice" role="status">
            Model settings apply to the next turn. The current response keeps
            its settings.
          </p>
        )}
        {active && !canQueueSettings && (
          <p className="notice">
            Available when this turn ends. Update the server to set options for
            the next turn.
          </p>
        )}
        {catalog.error && (
          <div role="alert">
            <p>{catalog.error}</p>
            <Button onClick={catalog.retry}>Retry model list</Button>
          </div>
        )}
        {legacyReceipt && (
          <div role="status">
            <p>
              Previous settings request has no account identity. Check the
              current settings.
            </p>
            <Button
              onClick={() => {
                save(legacyReceiptKey, null);
                setLegacyReceipt(false);
              }}
            >
              Discard previous settings request
            </Button>
          </div>
        )}
        {unconfirmed && !saving && (
          <div role="status">
            <p>
              The settings response is unconfirmed. Check the same save before
              changing settings again.
            </p>
            <Button
              disabled={saving}
              loading={saving}
              onClick={() =>
                void submit(unconfirmed, {
                  model: unconfirmed.model,
                  effort: unconfirmed.effort,
                  fast_mode: unconfirmed.fast_mode,
                })
              }
            >
              Check settings save
            </Button>
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
