import { Button, Popover, NativeSelect, Switch } from "@mantine/core";
import { ChevronDown } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, ApiError, errorText, save, saved } from "../api";
import { busy, type Agent, type Json } from "../types";
import {
  isDaybreakAlias,
  supportsDaybreakMode,
  type useWorkerModels,
} from "./WorkerModelPicker";
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
        "gpt-6-luna": "Luna",
        "gpt-daybreak-blue-latest": "Daybreak Blue",
        "gpt-daybreak-red-latest": "Daybreak Red",
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
const queuedFor = (agent: Agent) =>
  agent.pendingSettingsAccountKey &&
  agent.pendingSettingsAccountKey !== accountOf(agent)
    ? null
    : agent.pendingSettings;
const settingsFor = (agent: Agent, teamDefaults: boolean) => {
  if (teamDefaults)
    return {
      model:
        agent.workerDefaults?.model === undefined
          ? "gpt-6-luna"
          : agent.workerDefaults.model,
      effort:
        agent.workerDefaults?.effort === undefined
          ? "high"
          : agent.workerDefaults.effort,
      fast_mode: !!agent.workerDefaults?.fastMode,
      daybreak_enabled: !!agent.workerDefaults?.daybreakEnabled,
    };
  const queued = queuedFor(agent);
  return {
    model: queued?.model ?? agent.model,
    effort: queued ? (queued.effort ?? null) : (agent.effort ?? null),
    fast_mode: queued ? !!queued.fastMode : !!agent.fastMode,
    daybreak_enabled: !!(queued?.daybreakEnabled ?? agent.daybreakEnabled),
  };
};
const sameSettings = (left: Json, right: Json) =>
  left.model === right.model &&
  left.effort === right.effort &&
  left.fast_mode === right.fast_mode &&
  !!left.daybreak_enabled === !!right.daybreak_enabled;

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
  const receiptKey = `next-turn-settings:${JSON.stringify([agent.id, accountOf(agent)])}`;
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
  const queued = queuedFor(agent);
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
  }, [
    saving,
    pending,
    stored.model,
    stored.effort,
    stored.fast_mode,
    stored.daybreak_enabled,
  ]);
  const selectedModel = current.model || agent.model;
  const info = infoFor(catalog, selectedModel);
  const selectedProvider = teamDefaults ? info?.provider : agent.provider;
  const active = !teamDefaults && (!!agent.inFlight || busy.has(agent.status));
  const disabled =
    saving ||
    !!unconfirmed ||
    (active && !canQueueSettings) ||
    catalog.loading ||
    !!catalog.error;
  const models = catalog.models.filter((row) => !isDaybreakAlias(row.model));
  const daybreakEnabled = !!current.daybreak_enabled;
  const supportsMode = (row: Json | undefined, enabled = daybreakEnabled) =>
    supportsDaybreakMode(row, enabled);
  const canEnableDaybreak = models.some((row) => supportsMode(row, true));
  const canDisableDaybreak = models.some((row) => supportsMode(row, false));
  const modeSupported = supportsMode(info);
  const modeQueued =
    !teamDefaults &&
    !!(active || queued || unconfirmed) &&
    daybreakEnabled !== !!agent.daybreakEnabled;
  const modeLabel = daybreakEnabled ? "Daybreak" : "Standard";
  const modelOptions = models.map((row) => ({
    value: row.model as string,
    label: row.displayName || shortModel(row.model),
    disabled: !supportsMode(row),
  }));
  if (teamDefaults)
    modelOptions.unshift({
      value: DEFAULT,
      label: `Same as main agent (${agent.provider === "claude" ? infoFor(catalog, agent.model)?.displayName || shortModel(agent.model) : shortModel(agent.model)})`,
      disabled: !supportsMode(infoFor(catalog, agent.model)),
    });
  if (!modelOptions.some((row) => row.value === (current.model || DEFAULT)))
    modelOptions.unshift({
      value: current.model || DEFAULT,
      label: shortModel(selectedModel),
      disabled: true,
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
    if ("daybreak_enabled" in patch) {
      if (
        !supportsMode(
          infoFor(catalog, next.model || agent.model),
          next.daybreak_enabled,
        )
      ) {
        const eligible = models.filter((row) =>
          supportsMode(row, next.daybreak_enabled),
        );
        const replacement =
          eligible.find((row) => row.isDefault) || eligible[0];
        if (!replacement) {
          setError("No model supports this mode for the selected account.");
          return;
        }
        next.model = replacement.model;
        adjustments.push(
          `Model changed to ${shortModel(next.model)} for ${next.daybreak_enabled ? "Daybreak" : "standard mode"}.`,
        );
      }
    }
    if (
      !supportsMode(
        infoFor(catalog, next.model || agent.model),
        !!next.daybreak_enabled,
      )
    ) {
      setError(
        "Select a model that supports this mode for the selected account.",
      );
      return;
    }
    if ("model" in patch || next.model !== current.model) {
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
            `${modeLabel}${modeQueued ? " (next turn)" : ""}`,
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
          ·{" "}
          {selectedProvider === "claude"
            ? info?.displayName || shortModel(selectedModel)
            : shortModel(selectedModel)}
          {(daybreakEnabled || modeQueued) &&
            ` · ${modeLabel}${modeQueued ? " next turn" : ""}`}
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
        {selectedProvider !== "claude" && (
          <Switch
            label="Daybreak"
            aria-label="Daybreak"
            checked={daybreakEnabled}
            disabled={
              disabled ||
              (daybreakEnabled ? !canDisableDaybreak : !canEnableDaybreak)
            }
            description={
              canEnableDaybreak
                ? "Use Daybreak with supported models."
                : "Not available for this account."
            }
            onChange={(event) =>
              void change({ daybreak_enabled: event.currentTarget.checked })
            }
          />
        )}
        {!catalog.loading && !catalog.error && !modeSupported && (
          <p className="notice" role="status">
            {isDaybreakAlias(selectedModel)
              ? "This chat uses a Daybreak model alias. Select a model and enable Daybreak to use the separate mode."
              : "The selected model does not support this mode in the account model list. Select another model or change the mode."}
          </p>
        )}
        {selectedProvider !== "claude" &&
          !catalog.loading &&
          !catalog.error &&
          !canEnableDaybreak && (
            <Button variant="subtle" size="compact-xs" onClick={catalog.retry}>
              Refresh model list
            </Button>
          )}
        <NativeSelect
          label={prefix + " reasoning"}
          data={options}
          value={current.effort || DEFAULT}
          disabled={disabled || !modeSupported}
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
          disabled={
            disabled ||
            !modeSupported ||
            (!current.fast_mode && !fastTier(info))
          }
          description={
            fastTier(info)?.description || "Unavailable for this model"
          }
          onChange={(event) =>
            void change({ fast_mode: event.currentTarget.checked })
          }
        />
        {!teamDefaults && agent.isLead && agent.provider !== "claude" && (
          <details className="execution-permissions">
            <summary>Permissions</summary>
            <p>Applies to the whole team.</p>
            <Switch
              label="Full access without approval"
              aria-label="Full access without approval"
              checked={pendingYolo?.value ?? agent.yoloMode === true}
              disabled={saving || active || !("yoloMode" in agent)}
              description={
                !("yoloMode" in agent)
                  ? "Available after the server update."
                  : agent.yoloMode == null
                    ? "The team uses normal Codex permissions."
                    : "Tools run without permission prompts."
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
        {teamDefaults && <p className="notice">Default for new subagents.</p>}
        {((!teamDefaults && queued) || (active && canQueueSettings)) && (
          <p className="notice" role="status">
            Model settings apply to the next turn. The current response keeps
            its settings.
            {modeQueued &&
              ` Current mode: ${agent.daybreakEnabled ? "Daybreak" : "Standard"}. Next turn: ${modeLabel}.`}
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
                  daybreak_enabled: !!unconfirmed.daybreak_enabled,
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
