import { Button, Popover, NativeSelect, Switch } from "@mantine/core";
import { ChevronDown } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  post,
  ApiError,
  errorText,
  save,
  saved,
  type PostBody,
} from "../../api";
import { busy, type Agent, type Json, type JsonValue } from "../../types";
import {
  isDaybreakAlias,
  supportsDaybreakMode,
  useWorkerModels,
  type WorkerModelInfo,
} from "./WorkerModelPicker";
import { AccountTransferStatus, type Account } from "../Accounts";
import { ModelPicker, type ModelOption } from "../ModelPicker";
import "./execution-settings.css";
import { SettingsSection } from "../ui/primitives";

type Catalog = ReturnType<typeof useWorkerModels>;
type ConversationBody = PostBody<"/api/conversation">;
type SettingsValue = {
  account_key?: string | null;
  model: string | null;
  effort: string | null;
  fast_mode: boolean;
  daybreak_enabled: boolean;
};
type ReviewSettings = NonNullable<Agent["reviewDefaults"]>;
type SettingsAgent = Pick<
  Agent,
  | "accountKey"
  | "pendingSettingsAccountKey"
  | "pendingSettings"
  | "workerDefaults"
  | "model"
  | "effort"
  | "fastMode"
  | "daybreakEnabled"
>;
const DEFAULT = "__model_default__";
const title = (value: string) => value.charAt(0).toUpperCase() + value.slice(1);
const plural = (count: number, noun: string) =>
  `${count} ${noun}${count === 1 ? "" : "s"}`;
const infoFor = (catalog: Catalog, model: string) =>
  catalog.models.find(
    (row) => row.model === model || row.resolvedModel === model,
  );
const fastTier = (info?: WorkerModelInfo) =>
  info?.serviceTiers.find((tier) => tier.id === "priority");
const effortOptions = (info?: WorkerModelInfo) => [
  {
    value: DEFAULT,
    label: `Default${info?.defaultReasoningEffort ? ` (${info.defaultReasoningEffort})` : ""}`,
  },
  ...(info?.supportedReasoningEfforts || []).map((row) => ({
    value: row.reasoningEffort,
    label: title(row.reasoningEffort),
  })),
];
export function shortModel(model: string) {
  return (
    (
      {
        "gpt-6-astra": "Astra",
        "gpt-6.1-sol": "Sol 6.1",
        "gpt-6-sol": "Sol 6",
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
  accounts?: Account[];
  team?: Agent[];
  refresh: () => Promise<void>;
  teamDefaults?: boolean;
  nextTurnSupported?: boolean;
  onOpenChange?: (opened: boolean) => void;
  openRequest?: number;
  inline?: boolean;
  permissionsOnly?: boolean;
  permissionsTargetId?: string;
};
const accountOf = (agent: Pick<Agent, "accountKey">) =>
  agent.accountKey || "default";
const queuedFor = (agent: SettingsAgent) =>
  agent.pendingSettingsAccountKey &&
  agent.pendingSettingsAccountKey !== accountOf(agent)
    ? null
    : agent.pendingSettings;
const settingsFor = (
  agent: SettingsAgent,
  teamDefaults: boolean,
): SettingsValue => {
  if (teamDefaults)
    return {
      account_key: agent.workerDefaults?.accountKey ?? null,
      model:
        agent.workerDefaults?.model === undefined
          ? "gpt-6-luna"
          : (agent.workerDefaults.model ?? null),
      effort:
        agent.workerDefaults?.effort === undefined
          ? "high"
          : agent.workerDefaults.effort,
      fast_mode: !!agent.workerDefaults?.fastMode,
      daybreak_enabled: !!agent.workerDefaults?.daybreakEnabled,
    };
  const queued = queuedFor(agent);
  return {
    model: queued?.model ?? agent.model ?? null,
    effort: queued ? (queued.effort ?? null) : (agent.effort ?? null),
    fast_mode: queued ? !!queued.fastMode : !!agent.fastMode,
    daybreak_enabled: !!(queued?.daybreakEnabled ?? agent.daybreakEnabled),
  };
};
const settingsFromUnconfirmed = (request: ConversationBody): SettingsValue => {
  const defaults = request.worker_defaults;
  return {
    account_key: defaults?.account_key ?? null,
    model: request.model ?? defaults?.model ?? null,
    effort: request.effort ?? defaults?.effort ?? null,
    fast_mode: request.fast_mode ?? defaults?.fast_mode ?? false,
    daybreak_enabled:
      request.daybreak_enabled ?? defaults?.daybreak_enabled ?? false,
  };
};
const sameSettings = (left: SettingsValue, right: SettingsValue) =>
  left.account_key === right.account_key &&
  left.model === right.model &&
  left.effort === right.effort &&
  left.fast_mode === right.fast_mode &&
  !!left.daybreak_enabled === !!right.daybreak_enabled;
const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);
const isJsonValue = (value: unknown): value is JsonValue => {
  if (
    value === null ||
    typeof value === "string" ||
    typeof value === "boolean" ||
    (typeof value === "number" && Number.isFinite(value))
  )
    return true;
  if (Array.isArray(value)) return value.every(isJsonValue);
  return isRecord(value) && Object.values(value).every(isJsonValue);
};
const jsonObject = (value: unknown): Json | null => {
  if (!isRecord(value)) return null;
  const result: Json = {};
  for (const [key, item] of Object.entries(value)) {
    if (!isJsonValue(item)) return null;
    result[key] = item;
  }
  return result;
};

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
  catalog: parentCatalog,
  accounts = [],
  team = [],
  refresh,
  teamDefaults = false,
  nextTurnSupported,
  onOpenChange,
  openRequest = 0,
  inline = false,
  permissionsOnly = false,
  permissionsTargetId,
}: SettingsProps) {
  const [permissionsTarget, setPermissionsTarget] =
    useState<HTMLElement | null>(null);
  useLayoutEffect(() => {
    setPermissionsTarget(
      permissionsTargetId ? document.getElementById(permissionsTargetId) : null,
    );
  }, [permissionsTargetId]);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const canQueueSettings =
    nextTurnSupported ?? agent.nextTurnSettingsSupported === true;
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
      // An open model list handles its own Escape first.
      if (
        event.key === "Escape" &&
        (event.target as HTMLElement | null)?.getAttribute(
          "data-mantine-stop-propagation",
        ) !== "true"
      ) {
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
  const [unconfirmed, setUnconfirmed] = useState<ConversationBody | null>(
    () => {
      if (teamDefaults) return null;
      const value = saved<ConversationBody | null>(receiptKey, null);
      return value?.id === agent.id && value?.next_turn === true ? value : null;
    },
  );
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
    values: SettingsValue;
    baseline: SettingsValue;
  } | null>(null);
  const [reviewPending, setReviewPending] = useState<{
    values: ReviewSettings;
    baseline: ReviewSettings;
  } | null>(null);
  const [accountPending, setAccountPending] = useState<{
    target: string;
    baseline: string | null;
  } | null>(null);
  const [transfer, setTransfer] = useState<Json | null>(null);
  const [retryTarget, setRetryTarget] = useState<string | null>(null);
  const [error, setError] = useState("");
  const [status, setStatus] = useState<{
    kind: "saving" | "saved";
    text: string;
  } | null>(null);
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
  const reviewStored = {
    model: agent.reviewDefaults?.model ?? null,
    effort: agent.reviewDefaults?.effort ?? null,
  };
  const reviewCurrent = reviewPending?.values || reviewStored;
  const settled =
    (unconfirmed ? settingsFromUnconfirmed(unconfirmed) : null) ||
    pending?.values ||
    stored;
  const current = accountPending
    ? { ...settled, account_key: accountPending.target }
    : settled;
  const accountCatalog = useWorkerModels(
    current.account_key || accountOf(agent),
    teamDefaults && opened && !!current.account_key,
  );
  const catalog =
    teamDefaults && current.account_key ? accountCatalog : parentCatalog;
  const accountOptions = accounts
    .filter((account) => !account.disconnected && account.status === "ready")
    .map((account) => ({
      value: account.id,
      label: account.email || account.label || account.id,
    }));
  if (
    current.account_key &&
    !accountOptions.some((row) => row.value === current.account_key)
  )
    accountOptions.push({
      value: current.account_key,
      label: `${current.account_key} (unavailable)`,
    });
  const accountLabel = (key: string | null | undefined) => {
    const account = accounts.find((row) => row.id === key);
    return account?.email || account?.label || key || "Automatic";
  };
  useEffect(() => {
    // The optimistic account stays until the snapshot carries the change.
    if (
      !saving &&
      accountPending &&
      (stored.account_key === accountPending.target ||
        stored.account_key !== accountPending.baseline)
    )
      setAccountPending(null);
  }, [saving, accountPending, stored.account_key]);
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
    stored.account_key,
    stored.model,
    stored.effort,
    stored.fast_mode,
    stored.daybreak_enabled,
  ]);
  useEffect(() => {
    if (
      !saving &&
      reviewPending &&
      (JSON.stringify(reviewPending.values) === JSON.stringify(reviewStored) ||
        JSON.stringify(reviewPending.baseline) !== JSON.stringify(reviewStored))
    )
      setReviewPending(null);
  }, [saving, reviewPending, reviewStored.model, reviewStored.effort]);
  const selectedModel = current.model || agent.model || "";
  const info = infoFor(catalog, selectedModel);
  const selectedProvider = teamDefaults ? info?.provider : agent.provider;
  const active =
    !teamDefaults && (!!agent.inFlight || busy.has(agent.status ?? ""));
  const catalogKnown = !catalog.loading && !catalog.error;
  const disabled =
    saving ||
    !!unconfirmed ||
    (active && !canQueueSettings) ||
    (!teamDefaults && !catalogKnown);
  const models = catalog.models.filter((row) => !isDaybreakAlias(row.model));
  const daybreakEnabled = !!current.daybreak_enabled;
  const supportsMode = (
    row: WorkerModelInfo | undefined,
    enabled = daybreakEnabled,
  ) => supportsDaybreakMode(row, enabled);
  const canEnableDaybreak = models.some((row) => supportsMode(row, true));
  const canDisableDaybreak = models.some((row) => supportsMode(row, false));
  const modeSupported = supportsMode(info);
  const modeQueued =
    !teamDefaults &&
    !!(active || queued || unconfirmed) &&
    daybreakEnabled !== !!agent.daybreakEnabled;
  const modeLabel = daybreakEnabled ? "Daybreak" : "Standard";
  const modelOptions: ModelOption[] = models.map((row) => ({
    value: row.model,
    label: row.displayName || shortModel(row.model),
    description: row.description || undefined,
    isDefault: !!row.isDefault,
    disabled: !supportsMode(row),
  }));
  if (teamDefaults) {
    const leadModel = agent.model ?? "";
    const leadInfo = infoFor(catalog, leadModel);
    modelOptions.unshift({
      value: DEFAULT,
      label: `Same as main agent (${agent.provider === "claude" ? leadInfo?.displayName || shortModel(leadModel) : shortModel(leadModel)})`,
      description: leadInfo?.description || undefined,
      disabled: !supportsMode(leadInfo),
    });
  }
  if (!modelOptions.some((row) => row.value === (current.model || DEFAULT)))
    modelOptions.unshift({
      value: current.model || DEFAULT,
      label: shortModel(selectedModel),
      disabled: true,
    });
  // The current tag marks the model in use now, not a queued choice.
  const currentModel = teamDefaults ? stored.model || DEFAULT : agent.model;
  const options = effortOptions(info);
  if (current.effort && !options.some((row) => row.value === current.effort))
    options.push({ value: current.effort, label: title(current.effort) });
  const reviewModels = catalog.models.filter(
    (row) =>
      row.model.startsWith("gpt-") &&
      !isDaybreakAlias(row.model) &&
      supportsDaybreakMode(row, false),
  );
  const reviewInfo = reviewModels.find(
    (row) => row.model === reviewCurrent.model,
  );
  const reviewOptions: ModelOption[] = [
    {
      value: DEFAULT,
      label: "Same as caller",
      description: "Use the caller model and reasoning level.",
    },
    ...reviewModels.map((row) => ({
      value: row.model,
      label: row.displayName || shortModel(row.model),
      description: row.description || undefined,
      isDefault: !!row.isDefault,
    })),
  ];
  if (
    reviewCurrent.model &&
    !reviewOptions.some((row) => row.value === reviewCurrent.model)
  )
    reviewOptions.push({
      value: reviewCurrent.model,
      label: shortModel(reviewCurrent.model),
      disabled: true,
    });
  const reviewEfforts = effortOptions(reviewInfo);
  if (
    reviewCurrent.effort &&
    !reviewEfforts.some((row) => row.value === reviewCurrent.effort)
  )
    reviewEfforts.push({
      value: reviewCurrent.effort,
      label: title(reviewCurrent.effort),
    });
  const submit = async (
    request: ConversationBody,
    next: SettingsValue,
    notice = "",
  ) => {
    if (saveLock.current) return;
    saveLock.current = true;
    request = { ...request, expected_account_key: accountOf(agent) };
    const previousPending = unconfirmed ? null : pending;
    setPending({ values: next, baseline: stored });
    setSaving(true);
    setError("");
    setStatus({ kind: "saving", text: "" });
    if (request.next_turn) {
      save(receiptKey, request);
      setUnconfirmed(request);
    }
    const clearReceipt = () => {
      if (
        saved<ConversationBody | null>(receiptKey, null)?.request_id ===
        request.request_id
      )
        save(receiptKey, null);
    };
    let confirmed = false;
    try {
      // A lost response must not leave the control saving forever.
      const canonical = await post(
        "/api/conversation",
        request,
        request.next_turn || teamDefaults ? { timeoutMs: 15000 } : {},
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
      setStatus({ kind: "saved", text: notice });
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
      setStatus(null);
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
  const changeAccount = async (target: string | null) => {
    if (saveLock.current || target === current.account_key) return;
    if (!target) {
      await submit(
        { id: agent.id, worker_defaults: { ...current, account_key: null } },
        { ...current, account_key: null },
      );
      return;
    }
    // A lost response is retried with the same request id.
    const subagentTransferReceiptKey = `subagent-account-transfer:${JSON.stringify([agent.id, target])}`;
    const previous = saved<{ target?: string; request_id?: string } | null>(
      subagentTransferReceiptKey,
      null,
    );
    const requestId =
      previous && previous.target === target && previous.request_id
        ? previous.request_id
        : crypto.randomUUID();
    save(subagentTransferReceiptKey, { target, request_id: requestId });
    saveLock.current = true;
    setAccountPending({ target, baseline: stored.account_key ?? null });
    setSaving(true);
    setError("");
    setStatus({ kind: "saving", text: "" });
    try {
      const op = await post(
        "/api/agents/account-transfer",
        {
          id: agent.id,
          account_key: target,
          request_id: requestId,
          scope: "subagents",
        },
        { timeoutMs: 30000 },
      );
      save(subagentTransferReceiptKey, null);
      if (!mounted.current) return;
      const transferResult = jsonObject(op);
      setTransfer(
        transferResult ? { scope: "subagents", ...transferResult } : null,
      );
      setStatus({ kind: "saved", text: "" });
    } catch (failure) {
      if (!mounted.current) return;
      setAccountPending(null);
      setStatus(null);
      setError(errorText(failure));
      setRetryTarget(target);
      return;
    } finally {
      saveLock.current = false;
      if (mounted.current) setSaving(false);
    }
    setRetryTarget(null);
    void refresh().catch(() => {});
  };
  const providerOf = (member: Agent) =>
    member.provider ||
    accounts.find((row) => row.id === (member.accountKey || "default"))
      ?.provider ||
    "codex";
  const targetProvider = accountPending
    ? accounts.find((row) => row.id === accountPending.target)?.provider ||
      "codex"
    : "";
  const members = team.filter(
    (member) =>
      member.id !== agent.id && member.rootId === agent.id && !member.deletedAt,
  );
  const preview = {
    moving: members.filter((member) => providerOf(member) === targetProvider),
    staying: members.filter((member) => providerOf(member) !== targetProvider),
    stayingProvider: "",
  };
  preview.stayingProvider = preview.staying.length
    ? providerOf(preview.staying[0])
    : "";
  const checkedModel = useRef("");
  useEffect(() => {
    if (
      !teamDefaults ||
      !opened ||
      saving ||
      accountPending ||
      !stored.account_key ||
      !catalogKnown
    )
      return;
    const key = JSON.stringify([
      stored.account_key,
      stored.model,
      stored.daybreak_enabled,
    ]);
    if (checkedModel.current === key) return;
    checkedModel.current = key;
    const wanted = stored.model || agent.model || "";
    if (supportsMode(infoFor(catalog, wanted), !!stored.daybreak_enabled))
      return;
    const eligible = models.filter((row) => supportsMode(row, false));
    const replacement = eligible.find((row) => row.isDefault) || eligible[0];
    if (!replacement) return;
    const next = {
      ...stored,
      model: replacement.model,
      effort: null,
      fast_mode: false,
      daybreak_enabled: false,
    };
    void submit(
      { id: agent.id, worker_defaults: next },
      next,
      `${shortModel(wanted)} is not available on ${accountLabel(stored.account_key)}. Using ${replacement.displayName || shortModel(replacement.model)}, the account default.`,
    );
  });
  const storedTransfer = jsonObject(agent.accountTransfer);
  const snapshotTransfer =
    storedTransfer?.scope === "subagents" ? storedTransfer : null;
  // The snapshot summary is newer than the local response once it arrives.
  const shownTransfer = snapshotTransfer || transfer;
  const runTeamTransferAction = async (action: "retry" | "cancel") => {
    if (saveLock.current || typeof shownTransfer?.id !== "string") return;
    saveLock.current = true;
    setSaving(true);
    setError("");
    try {
      const op = await post("/api/agents/account-transfer", {
        action,
        scope: "subagents",
        request_id: shownTransfer.id,
      });
      const transferResult = jsonObject(op);
      if (mounted.current && transferResult)
        setTransfer({ scope: "subagents", ...transferResult });
      await refresh();
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      saveLock.current = false;
      setSaving(false);
    }
  };
  const change = async (patch: Partial<SettingsValue>) => {
    if (disabled) return;
    const next = { ...current, ...patch };
    const adjustments: string[] = [];
    if ("daybreak_enabled" in patch && catalogKnown) {
      if (
        !supportsMode(
          infoFor(catalog, next.model || agent.model || ""),
          !!next.daybreak_enabled,
        )
      ) {
        const eligible = models.filter((row) =>
          supportsMode(row, !!next.daybreak_enabled),
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
      catalogKnown &&
      !supportsMode(
        infoFor(catalog, next.model || agent.model || ""),
        !!next.daybreak_enabled,
      )
    ) {
      setError(
        "Select a model that supports this mode for the selected account.",
      );
      return;
    }
    if (catalogKnown && ("model" in patch || next.model !== current.model)) {
      const nextInfo = infoFor(catalog, next.model || agent.model || "");
      if (
        !nextInfo?.supportedReasoningEfforts?.some(
          (row) => row.reasoningEffort === next.effort,
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
    const request: ConversationBody = teamDefaults
      ? {
          id: agent.id,
          worker_defaults: {
            account_key: next.account_key,
            model: next.model,
            effort: next.effort,
            fast_mode: next.fast_mode,
            daybreak_enabled: next.daybreak_enabled,
          },
        }
      : {
          id: agent.id,
          model: next.model,
          effort: next.effort,
          fast_mode: next.fast_mode,
          daybreak_enabled: next.daybreak_enabled,
          ...(active || queued
            ? { next_turn: true, request_id: crypto.randomUUID() }
            : {}),
        };
    await submit(request, next, adjustments.join(" "));
  };
  const changeReview = async (patch: Partial<ReviewSettings>) => {
    if (disabled || saveLock.current) return;
    const next = { ...reviewCurrent, ...patch };
    if ("model" in patch) next.effort = null;
    saveLock.current = true;
    setReviewPending({ values: next, baseline: reviewStored });
    setSaving(true);
    setError("");
    setStatus({ kind: "saving", text: "" });
    try {
      const canonical = await post(
        "/api/conversation",
        {
          id: agent.id,
          expected_account_key: accountOf(agent),
          review_defaults: next,
        },
        { timeoutMs: 15000 },
      );
      if (
        !canonical ||
        canonical.id !== agent.id ||
        accountOf(canonical) !== accountOf(agent)
      )
        throw new Error(
          "The settings account changed. Check the current settings.",
        );
      if (!mounted.current) return;
      setReviewPending({
        values: {
          model: canonical.reviewDefaults?.model ?? null,
          effort: canonical.reviewDefaults?.effort ?? null,
        },
        baseline: reviewStored,
      });
      setStatus({ kind: "saved", text: "" });
      await refresh();
    } catch (failure) {
      if (!mounted.current) return;
      setReviewPending(null);
      setStatus(null);
      setError(errorText(failure));
    } finally {
      saveLock.current = false;
      if (mounted.current) setSaving(false);
    }
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
      const canonical = await post("/api/conversation", {
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
  const controls = (
    <>
      <ModelPicker
        id={teamDefaults ? undefined : "model"}
        label={prefix + " model"}
        visibleLabel={inline ? "Model" : undefined}
        options={modelOptions}
        value={current.model || DEFAULT}
        currentValue={currentModel || ""}
        disabled={disabled}
        onChange={(value) =>
          void change({ model: value === DEFAULT ? null : value })
        }
      />
      {selectedProvider !== "claude" &&
        !catalog.loading &&
        !catalog.error &&
        !canEnableDaybreak && (
          <Button
            className="model-refresh"
            variant="subtle"
            size="compact-xs"
            onClick={catalog.retry}
          >
            Refresh model list
          </Button>
        )}
      <NativeSelect
        label={inline ? "Reasoning" : prefix + " reasoning"}
        aria-label={prefix + " reasoning"}
        data={options}
        value={current.effort || DEFAULT}
        disabled={disabled || !modeSupported}
        description={
          !modeSupported ? "Unavailable for this model and mode." : undefined
        }
        onChange={(event) =>
          void change({
            effort:
              event.currentTarget.value === DEFAULT
                ? null
                : event.currentTarget.value,
          })
        }
      />
      {teamDefaults && (
        <NativeSelect
          label="Subagent account"
          description="Automatic uses the main account when it supports the model, then the application default."
          data={[{ value: "", label: "Automatic" }, ...accountOptions]}
          value={current.account_key || ""}
          disabled={saving}
          onChange={(event) =>
            void changeAccount(event.currentTarget.value || null)
          }
        />
      )}
      {teamDefaults && accountPending && (
        <p className="notice" role="status">
          {preview.moving.length
            ? `Moving ${plural(preview.moving.length, "subagent")} to ${accountLabel(accountPending.target)}.`
            : "No subagents to move."}
          {preview.staying.length > 0 &&
            ` ${plural(preview.staying.length, "subagent")} ${preview.staying.length === 1 ? "stays" : "stay"} on ${preview.stayingProvider}: ${preview.staying
              .map((member) => member.name)
              .join(", ")}.`}
        </p>
      )}
      {teamDefaults && !accountPending && shownTransfer && (
        <AccountTransferStatus
          transfer={shownTransfer}
          showCompleted
          targetLabel={accountLabel(
            typeof shownTransfer.targetAccountKey === "string"
              ? shownTransfer.targetAccountKey
              : undefined,
          )}
          pending={saving}
          onAction={(action) => void runTeamTransferAction(action)}
        />
      )}
      {teamDefaults && agent.provider !== "claude" && (
        <>
          <ModelPicker
            id="review-model"
            label="Default review model"
            description="Same as caller uses the model of the agent that requests the review."
            options={reviewOptions}
            value={reviewCurrent.model || DEFAULT}
            disabled={disabled}
            onChange={(value) =>
              void changeReview({ model: value === DEFAULT ? null : value })
            }
          />
          <NativeSelect
            label="Default review reasoning"
            data={reviewEfforts}
            value={reviewCurrent.effort || DEFAULT}
            disabled={disabled || !reviewCurrent.model || !reviewInfo}
            description={
              !reviewCurrent.model
                ? "Uses the caller’s reasoning when no review model is selected."
                : undefined
            }
            onChange={(event) =>
              void changeReview({
                effort:
                  event.currentTarget.value === DEFAULT
                    ? null
                    : event.currentTarget.value,
              })
            }
          />
        </>
      )}
      <SettingsSection title="Optional modes">
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
      </SettingsSection>
      {teamDefaults && (
        <p className="notice">
          For new subagents. An account change also moves existing ones.
        </p>
      )}
      {status && (
        <p role="status" className="notice execution-status">
          {status.kind === "saving"
            ? "Saving…"
            : `Saved${status.text ? ` · ${status.text}` : ""}`}
        </p>
      )}
      {((!teamDefaults && queued) || (active && canQueueSettings)) && (
        <p className="notice" role="status">
          Model settings apply to the next turn. The current response keeps its
          settings.
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
              void submit(unconfirmed, settingsFromUnconfirmed(unconfirmed))
            }
          >
            Check settings save
          </Button>
        </div>
      )}
      {error && (
        <p className="execution-error" role="alert">
          {error}
          {retryTarget && (
            <Button
              variant="subtle"
              size="compact-xs"
              disabled={saving}
              onClick={() => void changeAccount(retryTarget)}
            >
              Retry
            </Button>
          )}
        </p>
      )}
    </>
  );
  const permissions = (
    <>
      {!teamDefaults && agent.isLead && agent.provider !== "claude" && (
        <SettingsSection title="Permissions">
          <p>Applies to the whole team.</p>
          <Switch
            label="Full access without approval"
            aria-label="Full access without approval"
            checked={pendingYolo?.value ?? agent.yoloMode === true}
            disabled={saving || active || !("yoloMode" in agent)}
            description={
              !("yoloMode" in agent)
                ? "Available after the server update."
                : active
                  ? "Available when this turn ends."
                  : agent.yoloMode == null
                    ? "The team uses normal Codex permissions."
                    : "Tools run without permission prompts."
            }
            onChange={(event) => void changeYolo(event.currentTarget.checked)}
          />
          {saving && <p role="status">Saving…</p>}
          {error && (
            <p className="execution-error" role="alert">
              {error}
            </p>
          )}
        </SettingsSection>
      )}
    </>
  );
  const trigger = (
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
        setStatus(null);
        setOpened(!opened);
      }}
    >
      {inline && (
        <span className="execution-role">
          {teamDefaults ? "Subagents" : "Main agent"}
        </span>
      )}
      <span className="execution-selected">
        {selectedProvider === "claude"
          ? info?.displayName || shortModel(selectedModel)
          : shortModel(selectedModel)}
        {(daybreakEnabled || modeQueued) &&
          ` · ${modeLabel}${modeQueued ? " next turn" : ""}`}
      </span>
    </Button>
  );
  if (permissionsOnly) return permissions;
  if (inline)
    return (
      <div className="execution-inline">
        {trigger}
        {permissionsTarget && createPortal(permissions, permissionsTarget)}
        {opened && (
          <div
            className="execution-inline-body"
            role="region"
            aria-label={label}
          >
            {controls}
          </div>
        )}
      </div>
    );
  return (
    <Popover
      opened={opened}
      onChange={setOpened}
      position="top-start"
      width={340}
      shadow="md"
      middlewares={{ flip: true, shift: true, size: true }}
      closeOnEscape={false}
      trapFocus
      returnFocus
    >
      <Popover.Target>{trigger}</Popover.Target>
      <Popover.Dropdown
        className="execution-dropdown"
        role="dialog"
        aria-label={label}
      >
        {controls}
      </Popover.Dropdown>
    </Popover>
  );
}
