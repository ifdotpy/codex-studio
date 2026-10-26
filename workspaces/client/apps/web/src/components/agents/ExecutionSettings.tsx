import { settingsRequestId } from "../settingsRequestId";
import {
  isModelWhitelisted,
  useModelWhitelist,
} from "../models/modelWhitelist";
import { useServerApi } from "../../servers/ServerServicesContext";
import { accountCanStart } from "../../accountName";
import {
  Button,
  Popover,
  Drawer,
  Switch,
  SegmentedControl,
  NativeSelect,
  Stack,
  Text,
} from "@mantine/core";
import { ChevronDown } from "lucide-react";
import {
  type ReactElement,
  type ReactNode,
  type RefObject,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { createPortal } from "react-dom";
import { ApiError, errorText, type PostBody } from "../../api";
import { busy, type Agent, type Json, type JsonValue } from "../../types";
import {
  isDaybreakAlias,
  useModelCatalogReader,
  readWorkerModels,
  supportsDaybreakMode,
  useWorkerModels,
  type WorkerModelInfo,
} from "./WorkerModelPicker";
import { AccountTransferStatus, type Account } from "../Accounts";
import { layerZIndex } from "../../theme";
import {
  INHERITED_MODEL as DEFAULT,
  modelDisplayName,
  modelForValue,
  toModelOptions,
  type ModelOption,
} from "../models/modelOption";
import { ProviderMark, setupAccountName } from "../AccountTiles";
import { useMediaQuery } from "@mantine/hooks";
import { AgentSetupPicker, type SetupRole } from "./AgentSetupPicker";
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
  delegation_depth: number;
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
const title = (value: string) => value.charAt(0).toUpperCase() + value.slice(1);
const infoFor = (catalog: Catalog, model: string) =>
  modelForValue(catalog.models, model);
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
    if (target) return modelDisplayName(target, target.model);
  }
  if (!info && model === "default") return "Auto";
  return modelDisplayName(info, model);
};
const fastTier = (info?: WorkerModelInfo) =>
  info?.serviceTiers?.find((tier) => tier.id === "priority");

type SettingsProps = {
  presentation?: "menu" | "panel";
  renderTrigger?: (props: {
    agent: Agent;
    opened: boolean;
    ref: RefObject<HTMLButtonElement | null>;
    onClick: () => void;
  }) => ReactElement;
  agent: Agent;
  lead?: Agent;
  catalog: Catalog;
  accounts?: Account[];
  team?: Array<Pick<Agent, "id" | "rootId" | "deletedAt" | "accountKey">>;
  refresh: () => Promise<void>;
  initialRole?: SetupRole;
  settingsRow?: boolean;
  showAccountSummary?: boolean;
  extrasTargetId?: string;
  nextTurnSupported?: boolean;
  onOpenChange?: (opened: boolean) => void;
  openRequest?: number;
  inline?: boolean;
  permissionsTargetId?: string;
  onAccountChange?: (key: string) => void;
  accountDisabled?: boolean;
  accountModalOpen?: boolean;
  onDraftChange?: (patch: Partial<ConversationBody>) => void;
};
const accountOf = (agent: Pick<Agent, "accountKey">) =>
  agent.accountKey || "default";
// Codex accounts carry no provider field.
const providerOf = (account?: Pick<Account, "provider">) =>
  account ? account.provider || "codex" : undefined;
const queuedFor = (agent: SettingsAgent) =>
  agent.pendingSettingsAccountKey &&
  agent.pendingSettingsAccountKey !== accountOf(agent)
    ? null
    : agent.pendingSettings;
export const settingsFor = (
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
      delegation_depth: agent.workerDefaults?.delegationDepth ?? 1,
    };
  const queued = queuedFor(agent);
  return {
    model: queued?.model ?? agent.model ?? null,
    effort: queued ? (queued.effort ?? null) : (agent.effort ?? null),
    fast_mode: queued ? !!queued.fastMode : !!agent.fastMode,
    daybreak_enabled: !!(queued?.daybreakEnabled ?? agent.daybreakEnabled),
    delegation_depth: 1,
  };
};
const sameSettings = (left: SettingsValue, right: SettingsValue) =>
  left.account_key === right.account_key &&
  left.model === right.model &&
  left.effort === right.effort &&
  left.fast_mode === right.fast_mode &&
  !!left.daybreak_enabled === !!right.daybreak_enabled &&
  left.delegation_depth === right.delegation_depth;
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
  scope: "team" | "subagents",
): Json => {
  const operation = jsonObject(value);
  const receipt = jsonObject(jsonObject(operation?.requests)?.[requestId]);
  if (
    !operation ||
    typeof operation.id !== "string" ||
    operation.leadId !== leadId ||
    operation.targetAccountKey !== target ||
    operation.scope !== scope ||
    !["pending", "completed", "cancelled"].includes(String(operation.status)) ||
    (operation.id !== requestId &&
      (!receipt ||
        receipt.leadId !== leadId ||
        receipt.targetAccountKey !== target ||
        receipt.scope !== scope))
  )
    throw new Error("The server did not confirm this account transfer.");
  return operation;
};

export function ExecutionSettings(props: SettingsProps) {
  return <ExecutionSettingsController {...props} />;
}

// These fields belong to the Team section, outside the complete model picker.
export function DelegationSettings(props: SettingsProps) {
  return (
    <ExecutionSettingsController
      {...props}
      initialRole="worker"
      section="delegation"
    />
  );
}
export function ExecutionPermissions(props: SettingsProps) {
  return <ExecutionSettingsController {...props} section="permissions" />;
}
type ControllerProps = SettingsProps & {
  section?: "delegation" | "permissions";
};
type SettingsReceipt = { version: object; value: Agent };
const sameRoleSettings = (left: Agent, right: Agent, role: SetupRole) =>
  role === "review"
    ? left.reviewDefaults?.model === right.reviewDefaults?.model &&
      left.reviewDefaults?.effort === right.reviewDefaults?.effort
    : sameSettings(
        settingsFor(left, role === "worker"),
        settingsFor(right, role === "worker"),
      );
function ExecutionSettingsController(props: ControllerProps) {
  const [confirmed, setConfirmed] = useState<
    Partial<Record<SetupRole, SettingsReceipt>>
  >({});
  const projected = useRef({
    agent: props.agent,
    versions: { orchestrator: {}, worker: {}, review: {} },
    requests: {} as Partial<Record<SetupRole, object>>,
  });
  const previous = projected.current.agent;
  const changedScope =
    previous.id !== props.agent.id ||
    accountOf(previous) !== accountOf(props.agent);
  // A separate identity for each written settings domain lets unrelated SSE
  // updates coexist with a receipt. Any observed same-domain update wins,
  // including an update followed by a return to the earlier settings.
  for (const role of ["orchestrator", "worker", "review"] as const)
    if (changedScope || !sameRoleSettings(previous, props.agent, role)) {
      projected.current.versions[role] = {};
      delete projected.current.requests[role];
    }
  projected.current.agent = props.agent;
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const beginSettingsSave = (role: SetupRole) => {
    const version = projected.current.versions[role];
    const request = {};
    projected.current.requests[role] = request;
    return (value: Agent) => {
      const latest = projected.current;
      if (
        !mounted.current ||
        latest.versions[role] !== version ||
        latest.requests[role] !== request ||
        value.id !== latest.agent.id ||
        accountOf(value) !== accountOf(latest.agent)
      )
        return false;
      setConfirmed((old) => ({ ...old, [role]: { version, value } }));
      return true;
    };
  };
  const receipt = (role: SetupRole) => {
    const saved = confirmed[role];
    return saved?.version === projected.current.versions[role]
      ? saved.value
      : undefined;
  };
  const main = receipt("orchestrator");
  const worker = receipt("worker");
  const review = receipt("review");
  // Only execution fields bridge the response-to-SSE interval. Status and
  // other agent fields continue to come from the workspace projection.
  const triggerAgent = useMemo(
    () =>
      main || worker || review
        ? {
            ...props.agent,
            ...(main
              ? {
                  model: main.model,
                  effort: main.effort,
                  fastMode: main.fastMode,
                  daybreakEnabled: main.daybreakEnabled,
                  pendingSettings: main.pendingSettings,
                  pendingSettingsAccountKey: main.pendingSettingsAccountKey,
                }
              : {}),
            ...(worker ? { workerDefaults: worker.workerDefaults } : {}),
            ...(review ? { reviewDefaults: review.reviewDefaults } : {}),
          }
        : props.agent,
    [props.agent, main, worker, review],
  );
  const [role, setRole] = useState<SetupRole>(
    props.initialRole || (props.agent.isLead ? "orchestrator" : "worker"),
  );
  const [opened, setOpened] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if ((props.openRequest || 0) > 0) {
      triggerRef.current?.focus({ preventScroll: true });
      setRole(props.agent.isLead ? "orchestrator" : "worker");
      setOpened(true);
    }
  }, [props.openRequest]);
  const changeOpened = useCallback(
    (value: boolean) => {
      setOpened(value);
      if (!value) triggerRef.current?.focus({ preventScroll: true });
      if (!value && props.settingsRow)
        setRole(
          props.initialRole || (props.agent.isLead ? "orchestrator" : "worker"),
        );
    },
    [props.settingsRow, props.initialRole, props.agent.isLead],
  );
  const inheritedRole = !props.agent.isLead && role !== "worker";
  const agent = inheritedRole ? props.lead || props.agent : props.agent;
  const readOnlyReason = inheritedRole
    ? props.lead
      ? "Inherited from the lead. Open the lead chat to change these settings."
      : "Managed by the lead. Its settings are unavailable in this chat."
    : undefined;
  const roleSwitchId = useId();
  const focusRole = useRef(false);
  useLayoutEffect(() => {
    const selected = document
      .getElementById(roleSwitchId)
      ?.querySelector<HTMLInputElement>("input:checked");
    // Each role has isolated draft state. Preserve the selected keyboard
    // target when its form remounts inside the stable focus trap.
    selected?.setAttribute("data-autofocus", "true");
    if (focusRole.current) {
      selected?.focus();
      focusRole.current = false;
    }
  }, [role, roleSwitchId, opened]);
  const scope = JSON.stringify([agent.id, accountOf(agent), role]);
  const controls = (
    <ScopedExecutionSettings
      key={scope}
      {...props}
      beginSettingsSave={beginSettingsSave}
      presentation={props.renderTrigger ? "panel" : props.presentation}
      agent={agent}
      inheritedUnavailable={inheritedRole && !props.lead}
      readOnlyReason={readOnlyReason}
      roleSwitchId={roleSwitchId}
      teamDefaults={role === "review" || (role === "worker" && !!agent.isLead)}
      role={role}
      onRole={(value) => {
        focusRole.current = true;
        setRole(value);
      }}
      opened={opened}
      triggerRef={triggerRef}
      setOpened={changeOpened}
    />
  );
  if (!props.renderTrigger || props.section) return controls;
  return (
    <ExecutionMenu
      opened={opened}
      setOpened={changeOpened}
      accountModalOpen={props.accountModalOpen}
      label={settingsLabel(
        agent,
        role,
        role === "review" || (role === "worker" && !!agent.isLead),
      )}
      trigger={props.renderTrigger({
        agent: triggerAgent,
        opened,
        ref: triggerRef,
        onClick: () => changeOpened(!opened),
      })}
    >
      {controls}
    </ExecutionMenu>
  );
}

function ScopedExecutionSettings({
  agent,
  presentation = "menu",
  renderTrigger,
  section,
  readOnlyReason,
  inheritedUnavailable,
  roleSwitchId,
  catalog: parentCatalog,
  accounts = [],
  team,
  refresh,
  teamDefaults = false,
  nextTurnSupported,
  onOpenChange,
  initialRole = "orchestrator",
  settingsRow = false,
  showAccountSummary = false,
  extrasTargetId,
  permissionsTargetId,
  onAccountChange,
  accountDisabled = false,
  accountModalOpen = false,
  onDraftChange,
  role,
  onRole,
  opened,
  triggerRef,
  setOpened,
  beginSettingsSave,
}: ControllerProps & {
  beginSettingsSave: (role: SetupRole) => (agent: Agent) => boolean;
  teamDefaults: boolean;
  readOnlyReason?: string;
  inheritedUnavailable: boolean;
  roleSwitchId: string;
  role: SetupRole;
  onRole: (role: SetupRole) => void;
  opened: boolean;
  triggerRef: RefObject<HTMLButtonElement | null>;
  setOpened: (opened: boolean) => void;
}) {
  const { post, save, saved } = useServerApi();

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
      const cameFromModal = event
        .composedPath()
        .some(
          (target) =>
            target instanceof Element &&
            target.classList.contains("mantine-Modal-content"),
        );
      if (
        event.key === "Escape" &&
        !cameFromModal &&
        !accountModalOpen &&
        (event.target as HTMLElement | null)?.getAttribute(
          "data-mantine-stop-propagation",
        ) !== "true"
      ) {
        event.preventDefault();
        event.stopPropagation();
        setOpened(false);
      }
    };
    document.addEventListener("keydown", close, true);
    return () => document.removeEventListener("keydown", close, true);
  }, [accountModalOpen, opened]);
  const [saving, setSaving] = useState(false);
  const saveLock = useRef(false);
  const refreshGeneration = useRef(0);
  const receiptKey = `next-turn-settings:${JSON.stringify([agent.id, accountOf(agent)])}`;
  const [unconfirmed, setUnconfirmed] = useState<ConversationBody | null>(
    () => {
      if (teamDefaults || onDraftChange) return null;
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
  const label = settingsLabel(agent, role, teamDefaults);
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
    teamDefaults && (opened || settingsRow || presentation === "panel"),
    !effectiveAccount,
  );
  const inheritedCatalog = useWorkerModels(
    accountOf(agent),
    !!readOnlyReason && role === "orchestrator",
  );
  const catalog = teamDefaults
    ? accountCatalog
    : readOnlyReason
      ? inheritedCatalog
      : parentCatalog;
  const allowedModels = useModelWhitelist();
  const connectedAccounts = accounts.filter((account) =>
    accountCanStart(account),
  );
  const activeAccount = accounts.find(
    (account) => account.id === (effectiveAccount || accountOf(agent)),
  );
  const automaticProvider =
    infoFor(catalog, current.model || agent.model || "")?.provider ||
    providerOf(activeAccount) ||
    agent.provider ||
    "codex";
  // The main agent's provider: "Same as main agent" exists only on its tab.
  const leadProvider =
    providerOf(accounts.find((account) => account.id === accountOf(agent))) ||
    agent.provider ||
    "codex";
  const [browsedProvider, setBrowsedProvider] = useState<string | null>(null);
  const pickerProvider =
    role === "review"
      ? "codex"
      : browsedProvider ||
        (effectiveAccount ? providerOf(activeAccount) : automaticProvider) ||
        "codex";
  const pickerProviders = [
    ...new Set(connectedAccounts.map((account) => account.provider || "codex")),
  ].filter((provider) => role !== "review" || provider === "codex");
  const chooseAccount = (key: string | null) => {
    if (teamDefaults) void changeAccount(key);
    else if (key) onAccountChange?.(key);
  };
  // Subagent settings apply to new subagents only. An explicit account lists its own
  // models on its provider's tab. Automatic, and any other provider's tab, list the
  // models of all ready accounts of that provider; the server picks the account for
  // each new subagent from the model.
  const crossProvider =
    role === "worker" &&
    teamDefaults &&
    !!effectiveAccount &&
    pickerProvider !== (activeAccount?.provider || "codex");
  const allWorkerModels = useWorkerModels(
    accountOf(agent),
    crossProvider && (opened || settingsRow || presentation === "panel"),
    true,
  );
  const catalogReader = useModelCatalogReader();
  const existingElsewhere = (team || []).filter(
    (member) =>
      member.rootId === agent.id &&
      member.id !== agent.id &&
      !member.deletedAt &&
      accountOf(member) !== current.account_key,
  );

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
    stored.delegation_depth,
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
    !!readOnlyReason ||
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
  const modelOptions = toModelOptions(models, {
    provider: selectedProvider || "codex",
    disabledReason: (row) =>
      supportsMode(row)
        ? undefined
        : `Unavailable in ${modeLabel.toLowerCase()} mode.`,
  });
  if (teamDefaults) {
    const leadModel = agent.model ?? "";
    const leadInfo = infoFor(catalog, leadModel);
    modelOptions.unshift({
      value: DEFAULT,
      label: `Same as main agent (${modelDisplayName(leadInfo, leadModel)})`,
      provider: leadProvider || "codex",
      badges: [],
      description: leadInfo?.description || undefined,
      disabledReason: supportsMode(leadInfo)
        ? undefined
        : `Unavailable in ${modeLabel.toLowerCase()} mode.`,
    });
  }
  if (!modelForValue(modelOptions, current.model || DEFAULT))
    modelOptions.unshift({
      value: current.model || DEFAULT,
      label: modelDisplayName(selectedModel),
      provider: selectedProvider || "codex",
      badges: [],
      disabledReason: "Unavailable on this account.",
    });
  const reviewModels = catalog.models.filter(
    (row) =>
      row.model.startsWith("gpt-") &&
      !isDaybreakAlias(row.model) &&
      supportsDaybreakMode(row, false),
  );
  const reviewInfo = modelForValue(reviewModels, reviewCurrent.model);
  const reviewOptions: ModelOption[] = [
    {
      value: DEFAULT,
      label: "Same as caller",
      provider: "codex",
      badges: [],
      description: "Use the caller model and reasoning level.",
    },
    ...toModelOptions(reviewModels),
  ];
  if (reviewCurrent.model && !modelForValue(reviewOptions, reviewCurrent.model))
    reviewOptions.push({
      value: reviewCurrent.model,
      label: modelDisplayName(reviewCurrent.model),
      provider: "codex",
      badges: [],
      disabledReason: "Unavailable for review.",
    });
  const submit = async (request: ConversationBody, notice = "") => {
    if (readOnlyReason) return;
    if (onDraftChange) {
      onDraftChange(request);
      return;
    }
    if (saveLock.current) return;
    saveLock.current = true;
    const acceptCanonical = beginSettingsSave(
      teamDefaults ? "worker" : "orchestrator",
    );
    request = { ...request, expected_account_key: accountOf(agent) };
    const previousPending = pending;
    setSaving(true);
    setError("");
    setStatus({ kind: "saving", text: "" });
    ++refreshGeneration.current;
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
      const accepted = acceptCanonical(canonical);
      if (!mounted.current) return;
      if (accepted)
        setPending({
          values: settingsFor(canonical, teamDefaults),
          baseline: stored,
        });
      setStatus({ kind: "saved", text: notice });
    } catch (failure) {
      const rejected =
        failure instanceof ApiError &&
        [400, 401, 403, 404, 409].includes(failure.status);
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
  // A new account for new subagents keeps the model when the account offers it, else
  // the account's recommended model. Existing subagents keep theirs.
  const changeAccount = async (target: string | null) => {
    if (saveLock.current || target === current.account_key) return;
    let offered: WorkerModelInfo[];
    try {
      offered = await readWorkerModels(
        catalogReader,
        target || accountOf(agent),
        !target,
      );
    } catch (failure) {
      if (mounted.current) setError(errorText(failure));
      return;
    }
    const usable = offered.filter(
      (row) =>
        !row.hidden &&
        isModelWhitelisted(
          {
            ...row,
            provider:
              row.provider ||
              providerOf(
                accounts.find(
                  (account) => account.id === (target || accountOf(agent)),
                ),
              ),
          },
          allowedModels,
        ) &&
        !isDaybreakAlias(row.model) &&
        supportsDaybreakMode(row, false),
    );
    const wanted = current.model;
    const kept = modelForValue(usable, wanted);
    const model = kept
      ? wanted
      : target
        ? (usable.find((row) => row.isDefault) || usable[0])?.model
        : null;
    if (target && !model) {
      setError("This account offers no model for subagents.");
      return;
    }
    const row = modelForValue(usable, model || agent.model);
    const effort = row?.supportedReasoningEfforts?.some(
      (item) => item.reasoningEffort === current.effort,
    )
      ? current.effort
      : row?.defaultReasoningEffort || null;
    await submit({
      id: agent.id,
      worker_defaults: {
        ...current,
        account_key: target,
        model: model || null,
        effort,
        fast_mode: current.fast_mode && !!fastTier(row),
        daybreak_enabled: false,
      },
    });
  };
  // Moving existing subagents is a separate, explicit action.
  const moveExisting = async (target: string) => {
    if (saveLock.current || onDraftChange) return;
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
      const transferResult = confirmedTransfer(
        op,
        requestId,
        agent.id,
        target,
        "subagents",
      );
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
      `${modelDisplayName(wanted)} is not available on ${setupAccountName(accounts.find((account) => account.id === stored.account_key))}. Using ${modelDisplayName(replacement)}, the account default.`,
    );
  });
  const storedTransfer = jsonObject(agent.accountTransfer);
  const snapshotTransfer =
    storedTransfer &&
    (teamDefaults
      ? storedTransfer.scope === "subagents"
      : storedTransfer.scope !== "subagents")
      ? storedTransfer
      : null;
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
  const shownTransferTarget = accounts.find(
    (account) => account.id === shownTransfer?.targetAccountKey,
  );
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
        scope: teamDefaults ? "subagents" : "team",
        request_id: shownTransfer.id,
      });
      const transferResult = confirmedTransfer(
        op,
        shownTransfer.id,
        agent.id,
        shownTransfer.targetAccountKey,
        teamDefaults ? "subagents" : "team",
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
          `Model changed to ${modelDisplayName(next.model)} for ${next.daybreak_enabled ? "Daybreak" : "standard mode"}.`,
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
            delegation_depth: next.delegation_depth,
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
    if (onDraftChange) {
      onDraftChange({ review_defaults: next });
      return;
    }
    saveLock.current = true;
    const previousPending = reviewPending;
    const acceptCanonical = beginSettingsSave("review");
    setSaving(true);
    setError("");
    setStatus({ kind: "saving", text: "" });
    ++refreshGeneration.current;
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
      const accepted = acceptCanonical(canonical);
      if (!mounted.current) return;
      if (accepted)
        setReviewPending({
          values: {
            model: canonical.reviewDefaults?.model ?? null,
            effort: canonical.reviewDefaults?.effort ?? null,
          },
          baseline: reviewStored,
        });
      setStatus({ kind: "saved", text: "" });
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
      const storageKey = `permissions:${agent.id}:${accountOf(agent)}`;
      const requestId = settingsRequestId({ saved, save }, storageKey, enabled);
      const canonical = await post("/api/conversation", {
        id: agent.id,
        request_id: requestId,
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
      save(storageKey, null);
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
  const chooseAutomaticModel = (model: string) => {
    const row = infoFor(allWorkerModels, model);
    const effort = row?.supportedReasoningEfforts?.some(
      (item) => item.reasoningEffort === current.effort,
    )
      ? current.effort
      : row?.defaultReasoningEffort || null;
    void submit({
      id: agent.id,
      worker_defaults: {
        ...current,
        account_key: null,
        model,
        effort,
        fast_mode: current.fast_mode && !!fastTier(row),
        daybreak_enabled: false,
      },
    });
  };
  const selectedOptions = role === "review" ? reviewOptions : modelOptions;
  const storedPickerModel =
    (role === "review" ? reviewCurrent.model : current.model) || DEFAULT;
  const pickerModel = crossProvider
    ? ""
    : modelForValue(selectedOptions, storedPickerModel)?.value ||
      storedPickerModel;
  const crossOptions = toModelOptions(
    allWorkerModels.models.filter(
      (row) =>
        !isDaybreakAlias(row.model) &&
        !row.hidden &&
        (row.provider || pickerProvider) === pickerProvider,
    ),
    {
      provider: pickerProvider,
      disabledReason: (row) =>
        supportsDaybreakMode(row, false)
          ? undefined
          : "Unavailable in standard mode.",
    },
  ).map((row) => ({ ...row, label: displayModel(allWorkerModels, row.value) }));
  const pickerModels = (
    crossProvider
      ? crossOptions
      : (role === "review" ? reviewOptions : modelOptions).filter((row) => {
          if (row.value === DEFAULT)
            return (
              role === "review" ||
              (role === "worker" && leadProvider === pickerProvider)
            );
          const model = infoFor(catalog, row.value);
          if (!model)
            return !!row.disabledReason && row.provider === pickerProvider;
          return (
            !!model &&
            !model.hidden &&
            (role === "review" ||
              (model.provider ||
                providerOf(activeAccount) ||
                agent.provider ||
                "codex") === pickerProvider)
          );
        })
  )
    .map((row) => ({
      ...row,
      label:
        row.value === DEFAULT
          ? role === "review"
            ? "Same as caller"
            : "Same as main agent"
          : crossProvider
            ? row.label
            : displayModel(catalog, row.value),
    }))
    // One row per model name: a default alias and the model it resolves to
    // read the same. Keep the current choice, else the recommended alias.
    .filter((row, index, rows) => {
      const same = rows.filter((other) => other.label === row.label);
      if (same.length < 2) return true;
      const keep =
        same.find((other) => other.value === pickerModel) ||
        same.find((other) => other.badges.includes("default")) ||
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
  const delegation =
    teamDefaults && agent.isLead && !readOnlyReason && !onDraftChange ? (
      <NativeSelect
        label="Delegation depth"
        className="execution-delegation"
        value={String(current.delegation_depth)}
        description="Existing workers keep their depth."
        title="Lowering blocks new delegation immediately. Existing descendants keep running."
        data={[
          { value: "1", label: "Lead only (1)" },
          { value: "2", label: "One nested level (2)" },
          { value: "3", label: "Two nested levels (3)" },
        ]}
        disabled={saving}
        onChange={(event) =>
          void change({ delegation_depth: Number(event.currentTarget.value) })
        }
      />
    ) : null;
  const controls = (
    <>
      <AgentSetupPicker
        retainCurrentModel={!onDraftChange}
        role={role}
        onRole={onRole}
        roleSwitchId={roleSwitchId}
        readOnlyReason={readOnlyReason}
        provider={pickerProvider}
        providers={pickerProviders}
        onProvider={setBrowsedProvider}
        accounts={inheritedUnavailable ? [] : connectedAccounts}
        accountKey={
          teamDefaults
            ? current.account_key ||
              // Automatic belongs to the tab of the current model's provider.
              (role === "worker" && automaticProvider !== pickerProvider
                ? "\u0000"
                : "")
            : accountOf(agent)
        }
        onAccount={chooseAccount}
        automaticAccount={role === "worker" && teamDefaults}
        accountDisabled={
          !!readOnlyReason ||
          saving ||
          accountDisabled ||
          (!teamDefaults && !onAccountChange)
        }
        isAccountDisabled={(account) =>
          !!(
            !teamDefaults &&
            agent.isLead &&
            shownTransfer?.status === "pending" &&
            shownTransfer.scope !== "subagents" &&
            account.id !== shownTransfer.targetAccountKey
          )
        }
        modelId={
          presentation !== "menu" ? `${agent.id}-${role}-model` : undefined
        }
        models={
          inheritedUnavailable
            ? [
                {
                  value: "unavailable",
                  label: "Inherited settings unavailable",
                  provider: pickerProvider,
                  badges: [],
                },
              ]
            : pickerModels
        }
        model={inheritedUnavailable ? "unavailable" : pickerModel}
        modelLabel={
          role === "review" ? "Default review model" : prefix + " model"
        }
        onModel={(value) => {
          if (crossProvider) chooseAutomaticModel(value);
          else if (role === "review")
            void changeReview({ model: value === DEFAULT ? null : value });
          else void change({ model: value === DEFAULT ? null : value });
        }}
        efforts={inheritedUnavailable ? [] : pickerEfforts}
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
        fastAvailable={!inheritedUnavailable && !!fastTier(info)}
        fast={current.fast_mode}
        onFast={() => void change({ fast_mode: !current.fast_mode })}
        daybreakAvailable={
          !inheritedUnavailable &&
          selectedProvider !== "claude" &&
          (daybreakEnabled || supportsMode(info, true))
        }
        daybreak={daybreakEnabled}
        onDaybreak={() => void change({ daybreak_enabled: !daybreakEnabled })}
      />
      {role === "worker" &&
        teamDefaults &&
        !readOnlyReason &&
        !onDraftChange && (
          <p className="notice">
            {crossProvider
              ? `A model here is for new subagents on any ready ${title(pickerProvider)} account. Choose an account above to use only that one.`
              : "For new subagents. Existing subagents keep their account and model."}
          </p>
        )}
      {role === "worker" &&
        teamDefaults &&
        !onDraftChange &&
        !!current.account_key &&
        !crossProvider &&
        existingElsewhere.length > 0 &&
        shownTransfer?.status !== "pending" && (
          <Button
            size="compact-xs"
            variant="subtle"
            disabled={saving}
            onClick={() => void moveExisting(current.account_key!)}
          >
            Move {existingElsewhere.length} existing subagent
            {existingElsewhere.length === 1 ? "" : "s"} to{" "}
            {setupAccountName(activeAccount)}
          </Button>
        )}
      {!readOnlyReason && (teamDefaults || agent.isLead) && shownTransfer && (
        <AccountTransferStatus
          transfer={shownTransfer}
          targetLabel={
            teamDefaults
              ? setupAccountName(shownTransferTarget)
              : shownTransferTarget?.email ||
                setupAccountName(shownTransferTarget)
          }
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
      {!readOnlyReason && unconfirmed && !saving && (
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
              onClick={() => void moveExisting(retryTarget)}
            >
              Retry
            </Button>
          )}
        </p>
      )}
    </>
  );
  const fullAccess = pendingYolo?.value ?? agent.yoloMode === true;
  const askingNextTurn =
    (!!agent.inFlight || busy.has(agent.status ?? "")) && !fullAccess;
  const permissions =
    agent.isLead && agent.provider !== "claude" ? (
      settingsRow ? (
        <div className="execution-setting">
          <Text size="sm" fw={500}>
            Permissions
          </Text>
          <SegmentedControl
            className="execution-setting-control"
            fullWidth
            aria-label="Full access without approval"
            value={fullAccess ? "full" : "ask"}
            disabled={saving || !("yoloMode" in agent)}
            data={[
              { value: "ask", label: "Ask" },
              { value: "full", label: "Full" },
            ]}
            onChange={(value) => void changeYolo(value === "full")}
          />
          <Text size="xs" c="dimmed" className="execution-setting-description">
            {!("yoloMode" in agent)
              ? "Available after the server update."
              : askingNextTurn
                ? "Ask policy changes apply from the next turn."
                : "Applies immediately to the whole team."}
          </Text>
          {error && (
            <p className="execution-error" role="alert">
              {error}
            </p>
          )}
        </div>
      ) : (
        <SettingsSection title="Permissions">
          <p>Applies to the whole team.</p>
          <Switch
            label="Full access without approval"
            aria-label="Full access without approval"
            checked={fullAccess}
            disabled={saving || !("yoloMode" in agent)}
            description={
              !("yoloMode" in agent)
                ? "Available after the server update."
                : askingNextTurn
                  ? "Ask policy changes apply from the next turn."
                  : agent.yoloMode == null
                    ? "The team uses normal Codex permissions."
                    : fullAccess
                      ? "Tools run without permission prompts."
                      : "Tools ask for permission."
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
  // Native reviews inherit the invoking Codex actor, which may be a child of this lead.
  const inheritedReview = summaryRole === "review" && !summaryReview.model;
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
        providerOf(activeAccount) ||
        agent.provider ||
        "codex";
  const summaryEffort =
    (summaryRole === "review"
      ? summaryReview.effort || mainSettings.effort
      : summary.effort) ||
    summaryInfo?.defaultReasoningEffort ||
    mainSettings.effort ||
    mainInfo?.defaultReasoningEffort;
  const summaryAccountName =
    summaryRole !== "review" &&
    (showAccountSummary ||
      connectedAccounts.filter(
        (account) => (account.provider || "codex") === summaryProvider,
      ).length > 1)
      ? setupAccountName(
          accounts.find((account) => account.id === summaryAccount),
        )
      : "";
  const summaryContent = (
    <>
      {!inheritedReview && <ProviderMark provider={summaryProvider} />}
      <span className="execution-role">
        {summaryRole === "worker"
          ? "Workers"
          : summaryRole === "review"
            ? "Review"
            : agent.isLead
              ? "Lead"
              : "Agent"}
      </span>
      <span className="execution-model">
        {inheritedReview
          ? "Same as caller"
          : displayModel(summaryCatalog, summaryModel)}
      </span>
      {!inheritedReview && summaryEffort && (
        <span className="execution-effort">{title(summaryEffort)}</span>
      )}
      {summaryAccountName && (
        <span className="execution-account">{summaryAccountName}</span>
      )}
    </>
  );
  const toggle = () => {
    if (settingsRow) onRole(initialRole);
    setError("");
    setStatus(null);
    setOpened(!opened);
  };
  const trigger = (
    <Button
      ref={triggerRef}
      className="execution-menu"
      rightSection={<ChevronDown size={14} />}
      title={[
        summaryRole === "worker"
          ? "Workers"
          : summaryRole === "review"
            ? "Review"
            : agent.isLead
              ? "Lead"
              : "Agent",
        inheritedReview
          ? "Same as caller"
          : displayModel(summaryCatalog, summaryModel),
        !inheritedReview && summaryEffort ? title(summaryEffort) : "",
        !inheritedReview ? title(summaryProvider) : "",
        summaryRole !== "review"
          ? setupAccountName(
              accounts.find((account) => account.id === summaryAccount),
            )
          : "",
      ]
        .filter(Boolean)
        .join(" · ")}
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
      onClick={toggle}
    >
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
        {summaryContent}
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
  if (section === "delegation")
    return (
      <Stack gap="sm">
        {delegation}
        {error && <p role="alert">{error}</p>}
      </Stack>
    );
  if (section === "permissions") return permissions;
  if (presentation === "panel")
    return (
      <div
        className={renderTrigger ? undefined : "execution-profile-body"}
        data-role={role}
      >
        {controls}
      </div>
    );
  return (
    <>
      {permissionsTarget && createPortal(permissions, permissionsTarget)}
      {extrasTarget && createPortal(extras, extrasTarget)}
      <ExecutionMenu
        opened={opened}
        setOpened={setOpened}
        accountModalOpen={accountModalOpen}
        label={label}
        trigger={trigger}
      >
        {controls}
      </ExecutionMenu>
    </>
  );
}

const settingsLabel = (agent: Agent, role: SetupRole, teamDefaults: boolean) =>
  role === "review"
    ? "Review settings"
    : teamDefaults
      ? "Subagent defaults"
      : agent.isLead
        ? "Main agent settings"
        : "Subagent settings";

/** Keep the anchor and focus trap mounted while the isolated role form changes. */
function ExecutionMenu({
  opened,
  setOpened,
  accountModalOpen = false,
  label,
  trigger,
  children,
}: {
  opened: boolean;
  setOpened: (value: boolean) => void;
  accountModalOpen?: boolean;
  label: string;
  trigger: ReactElement;
  children: ReactNode;
}) {
  const mobile = useMediaQuery("(max-width: 760px)");
  const popoverLabelId = useId();
  if (mobile)
    return (
      <>
        {trigger}
        <Drawer
          opened={opened}
          onClose={() => setOpened(false)}
          closeOnEscape={false}
          closeOnClickOutside={!accountModalOpen}
          trapFocus={!accountModalOpen}
          returnFocus
          position="bottom"
          size="auto"
          title={label}
          className="setup-sheet"
          withinPortal
        >
          <div className="execution-dropdown" role="region" aria-label={label}>
            {children}
          </div>
        </Drawer>
      </>
    );
  return (
    <>
      <Popover
        opened={opened}
        onChange={setOpened}
        closeOnClickOutside={!accountModalOpen}
        trapFocus={!accountModalOpen}
        zIndex={layerZIndex.popover}
        position="top-start"
        shadow="md"
        // Mantine's default max-content width would override the picker token.
        width="var(--studio-execution-dropdown-size)"
        middlewares={{ flip: true, shift: true, size: true }}
        closeOnEscape={false}
        returnFocus
      >
        <Popover.Target>{trigger}</Popover.Target>
        <Popover.Dropdown
          className="execution-dropdown"
          role="dialog"
          aria-labelledby={popoverLabelId}
          aria-label={label}
        >
          <span id={popoverLabelId} className="sr-only">
            {label}
          </span>
          {children}
        </Popover.Dropdown>
      </Popover>
    </>
  );
}
