import {
  Button,
  Popover,
  Drawer,
  Switch,
  SegmentedControl,
  Tooltip,
} from "@mantine/core";
import { ChevronDown } from "lucide-react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
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
import type { ModelOption } from "../ModelPicker";
import { ProviderMark, setupAccountName } from "../AccountTiles";
import { useMediaQuery } from "@mantine/hooks";
import {
  AgentSetupPicker,
  setupModelName,
  type SetupRole,
} from "./AgentSetupPicker";
import "./execution-settings.css";
import { SettingsSection, SettingsRow } from "../ui/primitives";

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
const infoFor = (catalog: Catalog, model: string) =>
  catalog.models.find(
    (row) => row.model === model || row.resolvedModel === model,
  );
/** Name a model row; a default alias shows the model it resolves to. */
const displayModel = (catalog: Catalog, model: string) => {
  const info = infoFor(catalog, model);
  if (info?.isDefault && info.resolvedModel) {
    const target = catalog.models.find(
      (row) =>
        row.model !== info.model &&
        (row.model === info.resolvedModel ||
          row.resolvedModel === info.resolvedModel),
    );
    if (target) return setupModelName(target, target.model);
  }
  if (!info && model === "default") return "Auto";
  return setupModelName(info, model);
};
const fastTier = (info?: WorkerModelInfo) =>
  info?.serviceTiers?.find((tier) => tier.id === "priority");
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
  initialRole?: SetupRole;
  settingsRow?: boolean;
  showAccountSummary?: boolean;
  extrasTargetId?: string;
  nextTurnSupported?: boolean;
  onOpenChange?: (opened: boolean) => void;
  openRequest?: number;
  inline?: boolean;
  permissionsOnly?: boolean;
  permissionsTargetId?: string;
  onAccountChange?: (key: string) => void;
  accountDisabled?: boolean;
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
const confirmedTransfer = (
  value: unknown,
  requestId: string,
  leadId: string,
  target: string,
): Json => {
  const operation = jsonObject(value);
  const receipt = jsonObject(jsonObject(operation?.requests)?.[requestId]);
  if (
    !operation ||
    typeof operation.id !== "string" ||
    operation.leadId !== leadId ||
    operation.targetAccountKey !== target ||
    operation.scope !== "subagents" ||
    !["pending", "completed", "cancelled"].includes(String(operation.status)) ||
    (operation.id !== requestId &&
      (!receipt ||
        receipt.leadId !== leadId ||
        receipt.targetAccountKey !== target ||
        receipt.scope !== "subagents"))
  )
    throw new Error("The server did not confirm this account transfer.");
  return operation;
};

export function ExecutionSettings(props: SettingsProps) {
  const [role, setRole] = useState<SetupRole>(
    props.initialRole || (props.teamDefaults ? "worker" : "orchestrator"),
  );
  const [opened, setOpened] = useState(false);
  useEffect(() => {
    if ((props.openRequest || 0) > 0) {
      setRole("orchestrator");
      setOpened(true);
    }
  }, [props.openRequest]);
  const changeOpened = useCallback(
    (value: boolean) => {
      setOpened(value);
      if (!value && props.settingsRow)
        setRole(props.initialRole || "orchestrator");
    },
    [props.settingsRow, props.initialRole],
  );
  const scope = JSON.stringify([props.agent.id, accountOf(props.agent), role]);
  return (
    <ScopedExecutionSettings
      key={scope}
      {...props}
      teamDefaults={role !== "orchestrator"}
      role={role}
      onRole={setRole}
      opened={opened}
      setOpened={changeOpened}
    />
  );
}

function ScopedExecutionSettings({
  agent,
  catalog: parentCatalog,
  accounts = [],
  refresh,
  teamDefaults = false,
  nextTurnSupported,
  onOpenChange,
  permissionsOnly = false,
  initialRole = "orchestrator",
  settingsRow = false,
  showAccountSummary = false,
  extrasTargetId,
  permissionsTargetId,
  onAccountChange,
  accountDisabled = false,
  role,
  onRole,
  opened,
  setOpened,
}: SettingsProps & {
  role: SetupRole;
  onRole: (role: SetupRole) => void;
  opened: boolean;
  setOpened: (opened: boolean) => void;
}) {
  const [permissionsTarget, setPermissionsTarget] =
    useState<HTMLElement | null>(null);
  useLayoutEffect(() => {
    setPermissionsTarget(
      permissionsTargetId ? document.getElementById(permissionsTargetId) : null,
    );
  }, [permissionsTargetId]);
  const [extrasTarget, setExtrasTarget] = useState<HTMLElement | null>(null);
  useLayoutEffect(() => {
    setExtrasTarget(
      extrasTargetId ? document.getElementById(extrasTargetId) : null,
    );
  }, [extrasTargetId]);
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
  const refreshGeneration = useRef(0);
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
  const [transfer, setTransfer] = useState<{
    value: Json;
    baseline: string;
  } | null>(null);
  const [retryTarget, setRetryTarget] = useState<string | null>(null);
  const [error, setError] = useState("");
  const [status, setStatus] = useState<{
    kind: "saving" | "saved" | "accepted";
    text: string;
  } | null>(null);
  const label =
    role === "review"
      ? "Review settings"
      : teamDefaults
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
  const current = pending?.values || stored;
  const refreshConfirmed = (
    generation: number,
    confirmation = "Settings saved.",
  ) => {
    void Promise.resolve()
      .then(refresh)
      .catch((failure) => {
        if (mounted.current && generation === refreshGeneration.current)
          setError(`${confirmation} ${errorText(failure)}`);
      });
  };
  const effectiveAccount =
    role === "review" ? agent.workerDefaults?.accountKey : current.account_key;
  const accountCatalog = useWorkerModels(
    effectiveAccount || accountOf(agent),
    teamDefaults && (opened || settingsRow),
    !effectiveAccount,
  );
  const catalog = teamDefaults ? accountCatalog : parentCatalog;
  const connectedAccounts = accounts.filter(
    (account) =>
      !account.disconnected && !account.deleted && account.status === "ready",
  );
  const activeAccount = accounts.find(
    (account) => account.id === (effectiveAccount || accountOf(agent)),
  );
  const automaticProvider =
    infoFor(catalog, current.model || agent.model || "")?.provider ||
    activeAccount?.provider ||
    agent.provider ||
    "codex";
  const [browsedProvider, setBrowsedProvider] = useState<string | null>(null);
  const pickerProvider =
    role === "review"
      ? "codex"
      : browsedProvider ||
        (effectiveAccount ? activeAccount?.provider : automaticProvider) ||
        "codex";
  const pickerProviders = [
    ...new Set(connectedAccounts.map((account) => account.provider || "codex")),
  ].filter((provider) => role !== "review" || provider === "codex");
  const chooseAccount = (key: string | null) => {
    if (teamDefaults) void changeAccount(key);
    else if (key) onAccountChange?.(key);
  };
  useEffect(() => {
    // Keep the confirmed default until the snapshot carries the change.
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
  const submit = async (request: ConversationBody, notice = "") => {
    if (saveLock.current) return;
    saveLock.current = true;
    request = { ...request, expected_account_key: accountOf(agent) };
    const previousPending = pending;
    setSaving(true);
    setError("");
    setStatus({ kind: "saving", text: "" });
    const generation = ++refreshGeneration.current;
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
      refreshConfirmed(generation);
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
      await submit({
        id: agent.id,
        worker_defaults: { ...current, account_key: null },
      });
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
    const previousPending = accountPending;
    setSaving(true);
    setError("");
    setStatus({ kind: "saving", text: "" });
    const generation = ++refreshGeneration.current;
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
      const transferResult = confirmedTransfer(op, requestId, agent.id, target);
      save(subagentTransferReceiptKey, null);
      if (!mounted.current) return;
      setAccountPending({ target, baseline: stored.account_key ?? null });
      setTransfer({
        value: transferResult,
        baseline: JSON.stringify(snapshotTransfer),
      });
      setStatus({ kind: "accepted", text: "Account transfer accepted." });
    } catch (failure) {
      if (!mounted.current) return;
      setAccountPending(previousPending);
      setStatus(null);
      setError(errorText(failure));
      setRetryTarget(target);
      return;
    } finally {
      saveLock.current = false;
      if (mounted.current) setSaving(false);
    }
    setRetryTarget(null);
    refreshConfirmed(generation, "Account transfer accepted.");
  };
  const checkedModel = useRef("");
  useEffect(() => {
    if (
      role !== "worker" ||
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
      `${shortModel(wanted)} is not available on ${setupAccountName(accounts.find((account) => account.id === stored.account_key))}. Using ${replacement.displayName || shortModel(replacement.model)}, the account default.`,
    );
  });
  const storedTransfer = jsonObject(agent.accountTransfer);
  const snapshotTransfer =
    storedTransfer?.scope === "subagents" ? storedTransfer : null;
  // Keep the confirmed response until replication changes its old summary.
  const responseIsNewer =
    transfer?.value.id === snapshotTransfer?.id &&
    typeof transfer?.value.updated === "number" &&
    typeof snapshotTransfer?.updated === "number" &&
    transfer.value.updated > snapshotTransfer.updated;
  const shownTransfer =
    transfer &&
    (responseIsNewer || transfer.baseline === JSON.stringify(snapshotTransfer))
      ? transfer.value
      : snapshotTransfer || transfer?.value;
  const runTeamTransferAction = async (action: "retry" | "cancel") => {
    if (
      saveLock.current ||
      typeof shownTransfer?.id !== "string" ||
      typeof shownTransfer.targetAccountKey !== "string"
    )
      return;
    saveLock.current = true;
    setSaving(true);
    setError("");
    const generation = ++refreshGeneration.current;
    try {
      const op = await post("/api/agents/account-transfer", {
        action,
        scope: "subagents",
        request_id: shownTransfer.id,
      });
      const transferResult = confirmedTransfer(
        op,
        shownTransfer.id,
        agent.id,
        shownTransfer.targetAccountKey,
      );
      if (mounted.current)
        setTransfer({
          value: transferResult,
          baseline: JSON.stringify(snapshotTransfer),
        });
      refreshConfirmed(generation, "Transfer updated.");
    } catch (failure) {
      if (mounted.current) setError(errorText(failure));
    } finally {
      saveLock.current = false;
      if (mounted.current) setSaving(false);
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
    await submit(request, adjustments.join(" "));
  };
  const changeReview = async (patch: Partial<ReviewSettings>) => {
    if (disabled || saveLock.current) return;
    const next = { ...reviewCurrent, ...patch };
    if ("model" in patch) next.effort = null;
    saveLock.current = true;
    const previousPending = reviewPending;
    setSaving(true);
    setError("");
    setStatus({ kind: "saving", text: "" });
    const generation = ++refreshGeneration.current;
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
      refreshConfirmed(generation);
    } catch (failure) {
      if (!mounted.current) return;
      setReviewPending(previousPending);
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
    setSaving(true);
    setError("");
    const generation = ++refreshGeneration.current;
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
      refreshConfirmed(generation);
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
  const mobile = useMediaQuery("(max-width: 760px)");
  const pickerModel =
    role === "review"
      ? reviewCurrent.model || DEFAULT
      : current.model || DEFAULT;
  const pickerModels = (role === "review" ? reviewOptions : modelOptions)
    .filter((row) => {
      if (row.value === DEFAULT) return role !== "orchestrator";
      const model = infoFor(catalog, row.value);
      return (
        !!model &&
        !model.hidden &&
        (role === "review" ||
          (model.provider ||
            activeAccount?.provider ||
            agent.provider ||
            "codex") === pickerProvider)
      );
    })
    .map((row) => ({
      ...row,
      label:
        row.value === DEFAULT
          ? role === "review"
            ? "Same as caller"
            : "Same as main agent"
          : displayModel(catalog, row.value),
    }))
    // One row per model name: a default alias and the model it resolves to
    // read the same. Keep the current choice, else the recommended alias.
    .filter((row, index, rows) => {
      const same = rows.filter((other) => other.label === row.label);
      if (same.length < 2) return true;
      const keep =
        same.find((other) => other.value === pickerModel) ||
        same.find((other) => other.isDefault) ||
        same[0];
      return keep === row;
    });
  const pickerInfo = role === "review" ? reviewInfo : info;
  const pickerEfforts = (pickerInfo?.supportedReasoningEfforts || []).map(
    (row) => ({
      value: row.reasoningEffort,
      label: title(row.reasoningEffort),
    }),
  );
  const selectedEffort =
    (role === "review" ? reviewCurrent.effort : current.effort) ||
    pickerInfo?.defaultReasoningEffort ||
    pickerEfforts[0]?.value ||
    "";
  const controls = (
    <>
      <AgentSetupPicker
        role={role}
        onRole={onRole}
        roles={
          agent.isLead ? ["orchestrator", "worker", "review"] : ["orchestrator"]
        }
        provider={pickerProvider}
        providers={pickerProviders}
        onProvider={setBrowsedProvider}
        accounts={connectedAccounts}
        accountKey={teamDefaults ? current.account_key || "" : accountOf(agent)}
        onAccount={chooseAccount}
        automaticAccount={role === "worker"}
        accountDisabled={
          saving || accountDisabled || (!teamDefaults && !onAccountChange)
        }
        models={pickerModels}
        model={pickerModel}
        modelLabel={
          role === "review" ? "Default review model" : prefix + " model"
        }
        onModel={(value) => {
          if (role === "review")
            void changeReview({ model: value === DEFAULT ? null : value });
          else void change({ model: value === DEFAULT ? null : value });
        }}
        efforts={pickerEfforts}
        effort={selectedEffort}
        effortLabel={
          role === "review" ? "Default review reasoning" : prefix + " reasoning"
        }
        onEffort={(value) => {
          if (role === "review") void changeReview({ effort: value });
          else void change({ effort: value });
        }}
        effortDisabled={role !== "review" && !modeSupported}
        disabled={disabled || catalog.loading || !!catalog.error}
        fastAvailable={!!fastTier(info)}
        fast={current.fast_mode}
        onFast={() => void change({ fast_mode: !current.fast_mode })}
        daybreakAvailable={
          selectedProvider !== "claude" &&
          (daybreakEnabled || supportsMode(info, true))
        }
        daybreak={daybreakEnabled}
        onDaybreak={() => void change({ daybreak_enabled: !daybreakEnabled })}
      />
      {role === "worker" && (
        <p className="notice">
          For new subagents. An account change also moves existing ones.
        </p>
      )}
      {teamDefaults && shownTransfer && (
        <AccountTransferStatus
          transfer={shownTransfer}
          targetLabel={setupAccountName(
            accounts.find(
              (account) => account.id === shownTransfer.targetAccountKey,
            ),
          )}
          showCompleted
          pending={saving}
          onAction={(action) => void runTeamTransferAction(action)}
        />
      )}
      {status && (
        <p role="status" className="notice execution-status">
          {status.kind === "saving"
            ? "Saving…"
            : status.kind === "accepted"
              ? status.text
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
            onClick={() => void submit(unconfirmed)}
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
  const permissionsActive = !!agent.inFlight || busy.has(agent.status ?? "");
  const permissions =
    agent.isLead && agent.provider !== "claude" ? (
      settingsRow ? (
        <SettingsRow label="Permissions">
          <Tooltip
            label={
              permissionsActive
                ? "Available when this turn ends."
                : !("yoloMode" in agent)
                  ? "Available after the server update."
                  : ""
            }
            disabled={!permissionsActive && "yoloMode" in agent}
          >
            <span>
              <SegmentedControl
                aria-label="Full access without approval"
                value={
                  (pendingYolo?.value ?? agent.yoloMode === true)
                    ? "full"
                    : "ask"
                }
                disabled={saving || permissionsActive || !("yoloMode" in agent)}
                data={[
                  { value: "ask", label: "Ask" },
                  { value: "full", label: "Full" },
                ]}
                onChange={(value) => void changeYolo(value === "full")}
              />
            </span>
          </Tooltip>
          {error && (
            <p className="execution-error" role="alert">
              {error}
            </p>
          )}
        </SettingsRow>
      ) : (
        <SettingsSection title="Permissions">
          <p>Applies to the whole team.</p>
          <Switch
            label="Full access without approval"
            aria-label="Full access without approval"
            checked={pendingYolo?.value ?? agent.yoloMode === true}
            disabled={saving || permissionsActive || !("yoloMode" in agent)}
            description={
              !("yoloMode" in agent)
                ? "Available after the server update."
                : permissionsActive
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
      )
    ) : null;
  const mainSettings =
    role === "orchestrator" ? current : settingsFor(agent, false);
  const mainModel = mainSettings.model || agent.model || "";
  const mainInfo = infoFor(parentCatalog, mainModel);
  const summaryRole = settingsRow ? initialRole : "orchestrator";
  const summary =
    summaryRole === "orchestrator"
      ? mainSettings
      : role === "worker"
        ? current
        : settingsFor(agent, true);
  const summaryReview = role === "review" ? reviewCurrent : reviewStored;
  const summaryModel =
    summaryRole === "review"
      ? summaryReview.model || mainModel
      : summary.model || mainModel;
  const summaryAccount =
    summaryRole === "orchestrator"
      ? accountOf(agent)
      : summary.account_key || accountOf(agent);
  const summaryWorkerCatalog = useWorkerModels(
    summaryAccount,
    settingsRow && summaryRole !== "orchestrator",
    !summary.account_key,
  );
  const summaryCatalog =
    summaryRole === "orchestrator" ? parentCatalog : summaryWorkerCatalog;
  const summaryInfo =
    infoFor(summaryCatalog, summaryModel) ||
    infoFor(parentCatalog, summaryModel);
  const summaryProvider =
    summaryRole === "review"
      ? "codex"
      : summaryInfo?.provider ||
        activeAccount?.provider ||
        agent.provider ||
        "codex";
  const summaryEffort =
    (summaryRole === "review"
      ? summaryReview.effort || mainSettings.effort
      : summary.effort) ||
    summaryInfo?.defaultReasoningEffort ||
    mainSettings.effort ||
    mainInfo?.defaultReasoningEffort;
  const trigger = (
    <Button
      className="execution-menu"
      rightSection={<ChevronDown size={14} />}
      aria-label={
        settingsRow
          ? initialRole === "worker"
            ? "Subagent defaults"
            : initialRole === "review"
              ? "Review settings"
              : agent.isLead
                ? "Main agent settings"
                : "Subagent settings"
          : label
      }
      aria-expanded={opened}
      onClick={() => {
        if (settingsRow) onRole(initialRole);
        setError("");
        setStatus(null);
        setOpened(!opened);
      }}
    >
      <ProviderMark provider={summaryProvider} />
      <span
        className="execution-selected"
        data-inherited={
          summaryRole === "review"
            ? !summaryReview.model || undefined
            : summaryRole === "worker"
              ? summary.model === null || undefined
              : undefined
        }
      >
        {displayModel(summaryCatalog, summaryModel)}
        {summaryEffort && ` · ${title(summaryEffort)}`}
        {summaryRole !== "review" &&
          (showAccountSummary ||
            connectedAccounts.filter(
              (account) => (account.provider || "codex") === summaryProvider,
            ).length > 1) &&
          ` · ${setupAccountName(accounts.find((account) => account.id === summaryAccount))}`}
      </span>
    </Button>
  );
  const extras = (
    <div className="settings-group">
      {selectedProvider !== "claude" && (
        <Switch
          label="Daybreak"
          aria-label="Daybreak"
          checked={daybreakEnabled}
          disabled={
            disabled ||
            (daybreakEnabled
              ? !supportsMode(info, false)
              : !supportsMode(info, true))
          }
          onChange={(event) =>
            void change({ daybreak_enabled: event.currentTarget.checked })
          }
        />
      )}
      <Switch
        label="Fast mode"
        aria-label="Fast mode"
        checked={current.fast_mode}
        disabled={
          disabled || !modeSupported || (!current.fast_mode && !fastTier(info))
        }
        onChange={(event) =>
          void change({ fast_mode: event.currentTarget.checked })
        }
      />
      {error && <p role="alert">{error}</p>}
      {unconfirmed && (
        <Button disabled={saving} onClick={() => void submit(unconfirmed)}>
          Check settings save
        </Button>
      )}
    </div>
  );
  if (permissionsOnly) return permissions;
  if (mobile)
    return (
      <>
        {trigger}
        {permissionsTarget && createPortal(permissions, permissionsTarget)}
        {extrasTarget && createPortal(extras, extrasTarget)}
        <Drawer
          opened={opened}
          onClose={() => setOpened(false)}
          position="bottom"
          size="auto"
          title={label}
          className="setup-sheet"
          withinPortal
        >
          <div className="execution-dropdown" role="region" aria-label={label}>
            {controls}
          </div>
        </Drawer>
      </>
    );
  return (
    <>
      {permissionsTarget && createPortal(permissions, permissionsTarget)}
      {extrasTarget && createPortal(extras, extrasTarget)}
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
    </>
  );
}
