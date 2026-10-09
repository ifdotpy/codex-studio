import { serverLocalStorage as localStorage } from "../servers/storage";
import { accountDisplayName } from "../accountName";
import { ProviderMark, remainingLimit } from "./AccountTiles";
import { ActionButton } from "./ui/primitives";
import { localDateTime } from "../local-time";
import ErrorDescription from "./ErrorDescription";
import ClaudeProfile from "./ClaudeProfile";
import ClaudeSignIn from "./ClaudeSignIn";
import CodexSignIn from "./CodexSignIn";
import NativeRuntimeStatus from "./NativeRuntimeStatus";
import { watchResourceReads } from "./watchResourceReads";
import { useAccountLimits } from "../usage/useAccountLimits";
import { accountLimits } from "../usage/accountUsage";
import { Button, Group, Menu, Modal, Stack, TextInput } from "@mantine/core";
import {
  Check,
  ChevronDown,
  ArrowRightLeft,
  Plus,
  RefreshCw,
  UserRound,
  Pencil,
  Ellipsis,
} from "lucide-react";
import {
  useCallback,
  useEffect,
  useRef,
  useMemo,
  useState,
  type SetStateAction,
  type ReactNode,
} from "react";
import { errorText, get, post, type GetResult } from "../api";
import type { Agent } from "../types";
import "./accounts.css";
import AccountSignIn from "./AccountSignIn";
import AccountManagerHost from "./AccountManagerHost";
import AccountAddDialog, { type AddAccountIntent } from "./AccountAddDialog";
import type { ServerCommand } from "../servers/navigation";
import { readBuckets, formatPercent } from "./Usage";

const DELETE_REQUESTS_KEY = "codex-studio-account-delete-requests-v1";

function deleteRequestsStorageKey(scope?: string) {
  return `${DELETE_REQUESTS_KEY}:${scope || "default"}`;
}

function storedDeleteRequest(scope: string | undefined, accountKey: string) {
  try {
    const requests = JSON.parse(
      localStorage.getItem(deleteRequestsStorageKey(scope)) || "{}",
    );
    return typeof requests[accountKey] === "string"
      ? (requests[accountKey] as string)
      : "";
  } catch {
    return "";
  }
}

function saveDeleteRequest(
  scope: string | undefined,
  accountKey: string,
  requestId: string | null,
) {
  try {
    const current = JSON.parse(
      localStorage.getItem(deleteRequestsStorageKey(scope)) || "{}",
    );
    const requests =
      current && typeof current === "object" && !Array.isArray(current)
        ? current
        : {};
    if (requestId) requests[accountKey] = requestId;
    else delete requests[accountKey];
    localStorage.setItem(
      deleteRequestsStorageKey(scope),
      JSON.stringify(requests),
    );
  } catch {
    // The live dialog still retains its exact request identity when storage is unavailable.
  }
}

export type AccountsState = GetResult<"/api/accounts">;
export type Account = AccountsState["accounts"][number];
export function useAccounts(stateDir?: string) {
  const [data, setAccountsData] = useState<AccountsState>({
    accounts: [],
    archivedAccounts: [],
    defaultAccountKey: "default",
    logins: [],
  });
  const [error, setError] = useState("");
  const latestRead = useRef(0);
  const scope = useRef(stateDir);
  scope.current = stateDir;
  const setData = useCallback((value: SetStateAction<AccountsState>) => {
    latestRead.current++;
    setAccountsData(value);
    setError("");
  }, []);
  const refresh = useCallback(async () => {
    const read = ++latestRead.current;
    try {
      const result = await get("/api/accounts");
      if (read !== latestRead.current || scope.current !== stateDir)
        return null;
      setAccountsData(result);
      setError("");
      return result;
    } catch (e) {
      if (read === latestRead.current && scope.current === stateDir)
        setError(errorText(e));
      return null;
    }
  }, [stateDir]);
  useEffect(() => {
    if (!stateDir) return;
    const stop = watchResourceReads(
      { kind: "accounts" },
      async () => {
        await refresh();
      },
      (failure) => setError(errorText(failure)),
    );
    return () => {
      latestRead.current++;
      stop();
    };
  }, [stateDir, refresh]);
  return useMemo(
    () => ({ data, setData, error, refresh, scope: stateDir }),
    [data, setData, error, refresh, stateDir],
  );
}

function AccountNameEditor({
  account,
  onSaved,
  disabled,
}: {
  account: Account;
  onSaved: (data: AccountsState) => void;
  disabled: boolean;
}) {
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(account.label);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const request = useRef<{ label: string; request_id: string } | null>(null);
  const lock = useRef(false);
  const fallback = account.email?.split("@")[0] || "Account";
  const save = async () => {
    if (lock.current || disabled) return;
    if ([...name.trim()].length > 32) {
      setError("Use at most 32 characters.");
      return;
    }
    if (request.current?.label !== name)
      request.current = { label: name, request_id: crypto.randomUUID() };
    lock.current = true;
    setBusy(true);
    setError("");
    try {
      const result = await post("/api/accounts/name", {
        account_key: account.id,
        ...request.current,
      });
      onSaved(result);
      request.current = null;
      setEditing(false);
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  };
  const cancel = () => {
    if (lock.current) return;
    setEditing(false);
    setError("");
  };
  return (
    <div className="account-name">
      {editing ? (
        <form
          className="account-name-form"
          onSubmit={(event) => {
            event.preventDefault();
            void save();
          }}
        >
          <TextInput
            label="Name"
            aria-label={`Name for ${account.email || account.id}`}
            placeholder={fallback}
            value={name}
            onChange={(event) => setName(event.currentTarget.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                event.preventDefault();
                event.stopPropagation();
                cancel();
              }
            }}
            disabled={busy || disabled}
            autoFocus
            error={error || undefined}
          />
          <div className="account-name-actions">
            <ActionButton
              type="submit"
              actionRole="primary"
              loading={busy}
              disabled={disabled}
            >
              Save
            </ActionButton>
            <ActionButton actionRole="quiet" onClick={cancel} disabled={busy}>
              Cancel
            </ActionButton>
          </div>
        </form>
      ) : (
        <>
          <ActionButton
            actionRole="quiet"
            leftSection={<Pencil size={14} />}
            aria-label={`Edit name for ${account.email || account.id}`}
            disabled={disabled}
            onClick={() => {
              setName(account.label);
              setError("");
              setEditing(true);
            }}
          >
            {accountDisplayName(account)}
          </ActionButton>
        </>
      )}
    </div>
  );
}

function AccountCapacity({
  account,
  opened,
  compact = false,
  card = false,
}: {
  account: Account;
  opened: boolean;
  compact?: boolean;
  card?: boolean;
}) {
  const limits = useAccountLimits(
    account.id,
    account.accountId,
    opened && account.status === "ready",
  );
  const limitsError = limits?.data ? null : limits?.error;
  if (account.status !== "ready" && !card) return null;
  const buckets = readBuckets(
    accountLimits(limits, account.id, account.accountId),
    Date.now() / 1000,
  );
  if (compact) {
    const bucket = buckets.find(
      (bucket) =>
        bucket.id === (account.provider === "claude" ? "claude" : "codex") ||
        bucket.data.limitId ===
          (account.provider === "claude" ? "claude" : "codex"),
    );
    const weekly = bucket?.windows.find((window) => window.label === "7d");
    const label =
      !limits && !limitsError
        ? "Weekly: loading…"
        : limitsError ||
            (!limits?.data && limits?.error) ||
            !weekly ||
            weekly.remaining === null
          ? "Weekly: unavailable"
          : weekly.expired
            ? "Weekly: awaiting update"
            : `Weekly: ${formatPercent(weekly.remaining)} left`;
    return <small className="account-weekly-limit">{label}</small>;
  }

  const remaining =
    account.status !== "ready" ||
    limitsError ||
    (!limits?.data && limits?.error)
      ? null
      : remainingLimit(buckets.flatMap((bucket) => bucket.windows));
  const details = (
    <>
      {!limits && !limitsError && <small>Reading limits…</small>}
      {(limitsError || (!limits?.data && limits?.error)) && (
        <ErrorDescription
          className="account-action-error"
          role="status"
          value={limitsError || (!limits?.data && limits?.error)}
        />
      )}
      {buckets.map((bucket) => (
        <div key={bucket.id} className="account-capacity-pool">
          <span>{/spark/i.test(bucket.name) ? "Spark" : bucket.name}</span>
          <div>
            {bucket.windows.map((window) => (
              <span key={window.label} className="account-capacity-window">
                <strong>
                  {window.label}{" "}
                  {window.remaining === null
                    ? "Unknown"
                    : window.expired
                      ? "Reset due"
                      : `${formatPercent(window.remaining)} left`}
                </strong>
                {window.reset && (
                  <time
                    dateTime={new Date(window.reset * 1000).toISOString()}
                    title={localDateTime(new Date(window.reset * 1000))}
                  >
                    {window.expired ? "Due " : "Resets "}
                    {localDateTime(new Date(window.reset * 1000), {
                      month: "short",
                      day: "numeric",
                      hour: "2-digit",
                      minute: "2-digit",
                    })}
                  </time>
                )}
              </span>
            ))}
          </div>
        </div>
      ))}
      {limits && !limitsError && !limits.error && !buckets.length && (
        <small>Limits unavailable</small>
      )}
    </>
  );
  return (
    <div
      className="account-capacity"
      aria-label={`Limits for ${account.email || account.label}`}
    >
      {card ? (
        <>
          <div className="account-limit-summary">
            <span>Remaining limit</span>
            <span>
              {remaining === null
                ? !limits && !limitsError && account.status === "ready"
                  ? "Reading limits…"
                  : "Unavailable"
                : `${formatPercent(remaining)} left`}
            </span>
          </div>
          <span
            className="account-limit-bar"
            role={remaining === null ? undefined : "meter"}
            aria-label="Remaining limit"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={remaining ?? undefined}
          >
            {remaining !== null && (
              <span
                style={{
                  width: `${remaining}%`,
                  background:
                    remaining > 50
                      ? "var(--mantine-color-green-6)"
                      : remaining >= 15
                        ? "var(--mantine-color-yellow-6)"
                        : "var(--mantine-color-red-6)",
                }}
              />
            )}
          </span>
          {account.status !== "ready" ? null : limitsError ||
            (!limits?.data && limits?.error) ? (
            details
          ) : (
            <details className="account-limit-details">
              <summary>Limit details</summary>
              {details}
            </details>
          )}
        </>
      ) : (
        details
      )}
    </div>
  );
}

export function AccountTransferStatus({
  transfer,
  targetLabel,
  pending,
  showCompleted = false,
  onAction,
}: {
  transfer?: NonNullable<Agent["accountTransfer"]>;
  targetLabel?: string;
  pending?: boolean;
  showCompleted?: boolean;
  onAction: (action: "retry" | "cancel") => void;
}) {
  if (!transfer) return null;
  const waiting = Number(transfer.waitingCount || 0);
  const left = Array.isArray(transfer.leftOnSource)
    ? transfer.leftOnSource
    : [];
  const blocked = Array.isArray(transfer.blocked) ? transfer.blocked : [];
  const interrupted = Array.isArray(transfer.interrupted)
    ? transfer.interrupted
    : [];
  if (
    transfer.status !== "pending" &&
    !left.length &&
    !blocked.length &&
    (!showCompleted ||
      !Number(transfer.moved ?? transfer.completed ?? 0) ||
      !targetLabel)
  )
    return null;
  return (
    <div className="account-menu-note" role="status">
      <ArrowRightLeft size={14} />
      <div>
        {Number(transfer.moved ?? transfer.completed ?? 0) > 0 &&
          targetLabel && (
            <div>
              Moved {transfer.moved ?? transfer.completed} subagents to{" "}
              {targetLabel}.
            </div>
          )}
        {transfer.status === "pending" && waiting > 0 && (
          <div>{waiting} waiting</div>
        )}
        {!!transfer.nativeHistoryPending && (
          <small>
            History transfer in progress: {transfer.nativeHistoryPending}{" "}
            remaining.
          </small>
        )}
        {!!transfer.movingNow && <small>{transfer.movingNow} moving now</small>}
        {transfer.waiting && <small>{transfer.waiting}</small>}
        {interrupted.map((member) => (
          <small key={String(member.id)}>
            {member.name || "Agent"}: {member.reason}
          </small>
        ))}
        {blocked.map((member) => (
          <small key={String(member.id)} role="alert">
            {member.name || "Agent"} blocked: {member.reason}
          </small>
        ))}
        {left.map((member) => (
          <small key={String(member.id)}>
            {member.name || "Agent"} left on source ({member.provider}):{" "}
            {member.reason}
          </small>
        ))}
        <div className="account-transfer-actions">
          {transfer.canRetry && (
            <Button
              size="compact-xs"
              variant="subtle"
              disabled={pending}
              onClick={() => onAction("retry")}
            >
              Retry
            </Button>
          )}
          {transfer.status === "pending" && (
            <Button
              size="compact-xs"
              variant="subtle"
              disabled={pending}
              onClick={() => onAction("cancel")}
            >
              Cancel remaining
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

export function AccountTransferConfirmation({
  opened,
  onClose,
  target,
  sourceProvider,
  extend = false,
  onConfirm,
}: {
  opened: boolean;
  onClose: () => void;
  target: Account | null;
  sourceProvider: string;
  extend?: boolean;
  onConfirm: () => Promise<void>;
}) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const lock = useRef(false);
  useEffect(() => {
    setError("");
  }, [target?.id, opened]);
  return (
    <Modal
      opened={opened}
      onClose={() => {
        if (!pending) onClose();
      }}
      title={extend ? "Add agents to this transfer" : "Transfer this chat"}
      closeOnClickOutside={!pending}
      closeOnEscape={!pending}
      withCloseButton={!pending}
    >
      <p>
        {extend
          ? "Add descendants to the pending team transfer to"
          : "Transfer this chat to"}{" "}
        <strong>{target?.email || target?.label}</strong>?
      </p>
      <p>
        Active turns move immediately and continue on the destination. Same
        provider subagents move too; other providers stay on their accounts.
      </p>
      {target && sourceProvider !== (target.provider || "codex") && (
        <p>The destination model uses the saved chat context.</p>
      )}
      <p>Applies to this chat only.</p>
      {error && <p role="alert">{error}</p>}
      <Button variant="default" disabled={pending} onClick={onClose}>
        Cancel
      </Button>
      <Button
        loading={pending}
        disabled={!target || pending}
        onClick={async () => {
          if (lock.current) return;
          lock.current = true;
          setPending(true);
          setError("");
          try {
            await onConfirm();
            onClose();
          } catch (failure) {
            setError(errorText(failure));
          } finally {
            lock.current = false;
            setPending(false);
          }
        }}
      >
        {extend ? "Add agents" : "Transfer chat"}
      </Button>
    </Modal>
  );
}

export default function Accounts({
  state,
  agent,
  accountKey,
  changeAccount,
  onError,
  projectAccountKeys,
  onModalOpenChange,
  managerOnly = false,
  renderPicker,
  externalCommand,
  onExternalCommandHandled,
  dialogsOnly = false,
}: {
  state: ReturnType<typeof useAccounts>;
  agent?: Agent;
  accountKey?: string;
  projectAccountKeys?: string[];
  onModalOpenChange?: (opened: boolean) => void;
  managerOnly?: boolean;
  renderPicker?: (
    selectAccount: (key: string) => void,
    disabled: boolean,
  ) => ReactNode;
  changeAccount?: (key: string) => Promise<void>;
  onError: (message: string) => void;
  externalCommand?: Extract<
    ServerCommand,
    { action: "account-sign-in" | "account-action" }
  > | null;
  onExternalCommandHandled?: () => void;
  dialogsOnly?: boolean;
}) {
  const [opened, setOpened] = useState(false);
  const [menuOpened, setMenuOpened] = useState(false);
  const [pending, setPending] = useState("");
  const [error, setError] = useState("");
  const [home, setHome] = useState("");
  const [adding, setAdding] = useState(false);
  const [claudeLogin, setClaudeLogin] = useState<Account | null>(null);
  const [codexLogin, setCodexLogin] = useState<Account | null>(null);
  const [transferChoice, setTransferChoice] = useState<{
    target: Account;
    sourceProvider: string;
    agentId: string;
    requestId: string;
    extend: boolean;
  } | null>(null);
  const [disconnectChoice, setDisconnectChoice] = useState<Account | null>(
    null,
  );
  const [deleteChoice, setDeleteChoice] = useState<Account | null>(null);
  const [deleteRequestId, setDeleteRequestId] = useState("");
  const [addOpen, setAddOpen] = useState(false);
  const [addIntent, setAddIntent] = useState<AddAccountIntent | null>(null);
  const [externalServerLabel, setExternalServerLabel] = useState("");
  const [renameChoice, setRenameChoice] = useState<Account | null>(null);
  const [renameLabel, setRenameLabel] = useState("");
  const handledCommand = useRef<ServerCommand | null>(null);
  const actionLock = useRef(false);
  const childModalOpen =
    (!managerOnly && opened) ||
    addOpen ||
    !!renameChoice ||
    !!transferChoice ||
    !!disconnectChoice ||
    !!deleteChoice ||
    !!claudeLogin ||
    !!codexLogin;
  useEffect(() => {
    onModalOpenChange?.(
      managerOnly
        ? !!addOpen ||
            !!renameChoice ||
            !!disconnectChoice ||
            !!deleteChoice ||
            !!claudeLogin ||
            !!codexLogin
        : childModalOpen,
    );
    return () => onModalOpenChange?.(false);
  }, [
    childModalOpen,
    managerOnly,
    disconnectChoice,
    deleteChoice,
    claudeLogin,
    codexLogin,
    addOpen,
    renameChoice,
    onModalOpenChange,
  ]);
  const accounts = state.data.accounts || [];
  const selected =
    accounts.find((a) => a.id === accountKey) ||
    state.data.archivedAccounts?.find((a) => a.id === accountKey);
  const pinned =
    !!agent &&
    (!agent.isLead || !agent.empty || !!agent.threadId || !!agent.inFlight);
  const owner = agent?.isLead ? agent : undefined;
  const transfer = owner?.accountTransfer;
  const teamTransfer = transfer?.scope === "subagents" ? undefined : transfer;
  const transferring = teamTransfer?.status === "pending";
  const transferTarget = accounts.find(
    (a) => a.id === teamTransfer?.targetAccountKey,
  );
  const title = selected?.email || selected?.label || "Codex account";
  const replacement = accounts.find(
    (account) =>
      account.id !== disconnectChoice?.id &&
      !account.disconnected &&
      account.status === "ready",
  );
  const disconnectsDefault =
    disconnectChoice?.id === state.data.defaultAccountKey;
  const deletesDefault = deleteChoice?.id === state.data.defaultAccountKey;
  const deleteReplacement = accounts.find(
    (account) =>
      account.id !== deleteChoice?.id &&
      !account.disconnected &&
      account.status === "ready",
  );
  const action = async (key: string, run: () => Promise<unknown>) => {
    if (actionLock.current) return;
    actionLock.current = true;
    setPending(key);
    setError("");
    try {
      await run();
    } catch (e) {
      setError(errorText(e));
      onError(errorText(e));
    } finally {
      actionLock.current = false;
      setPending("");
    }
  };
  useEffect(() => {
    if (!externalCommand || handledCommand.current === externalCommand) return;
    const command = externalCommand;
    handledCommand.current = command;
    setExternalServerLabel(
      command.action === "account-sign-in" ? command.serverLabel : "",
    );
    if (command.action === "account-sign-in" && command.forceAdd) {
      setAddIntent({
        provider: command.provider,
        email: command.email,
        label: command.label,
        serverLabel: command.serverLabel,
      });
      setAddOpen(true);
      onExternalCommandHandled?.();
      return;
    }
    void state.refresh().then((fresh) => {
      const current =
        fresh || (command.action === "account-sign-in" ? state.data : null);
      if (!current) {
        setError("Cannot load accounts from this server.");
        onExternalCommandHandled?.();
        return;
      }
      const account = current.accounts.find(
        (candidate) =>
          (candidate.provider === "claude" ? "claude" : "codex") ===
            command.provider &&
          (command.email
            ? candidate.email?.toLocaleLowerCase() ===
              command.email.toLocaleLowerCase()
            : candidate.label === command.label),
      );
      if (command.action === "account-sign-in") {
        if (account && !command.forceAdd) {
          if (account.provider === "claude") setClaudeLogin(account);
          else setCodexLogin(account);
        } else {
          setAddIntent({
            provider: command.provider,
            email: command.email,
            label: command.label,
            serverLabel: command.serverLabel,
          });
          setAddOpen(true);
        }
      } else if (!account) {
        setError("This account is no longer saved on this server.");
      } else if (command.operation === "rename") {
        setRenameChoice(account);
        setRenameLabel(account.label);
      } else if (command.operation === "default") {
        void action(`default:${account.id}`, async () => {
          state.setData(
            await post("/api/accounts/default", { account_key: account.id }),
          );
        });
      } else if (command.operation === "disconnect") {
        setDisconnectChoice(account);
      } else {
        const requestId = crypto.randomUUID();
        setDeleteRequestId(requestId);
        saveDeleteRequest(state.scope, account.id, requestId);
        setDeleteChoice(account);
      }
      onExternalCommandHandled?.();
    });
  }, [externalCommand]);
  const selectAccount = (key: string) => {
    const account = accounts.find((candidate) => candidate.id === key);
    if (
      !account ||
      actionLock.current ||
      pending ||
      account.status !== "ready" ||
      account.disconnected ||
      (pinned && !owner) ||
      (transferring && account.id !== teamTransfer?.targetAccountKey)
    )
      return;
    const extendTransfer =
      transferring && account.id === teamTransfer?.targetAccountKey;
    if (account.id === accountKey && !extendTransfer) return;
    if (pinned && owner)
      setTransferChoice({
        target: account,
        sourceProvider: selected?.provider || owner.provider || "codex",
        agentId: owner.id,
        requestId: crypto.randomUUID(),
        extend: !!extendTransfer,
      });
    else if (!pinned)
      void action(account.id, async () => {
        await changeAccount?.(account.id);
      });
  };
  useEffect(() => {
    if (!opened && !managerOnly) return;
    void state.refresh();
  }, [opened, managerOnly, state.refresh]);
  return (
    <>
      {codexLogin && (
        <CodexSignIn
          account={codexLogin}
          state={state}
          serverLabel={externalServerLabel || undefined}
          onClose={() => setCodexLogin(null)}
        />
      )}
      {claudeLogin && (
        <ClaudeSignIn
          key={claudeLogin.id}
          account={claudeLogin}
          scope={state.scope}
          serverLabel={externalServerLabel || undefined}
          onClose={() => setClaudeLogin(null)}
          onReady={async () => {
            if (claudeLogin.disconnected)
              state.setData(
                await post("/api/accounts/reconnect", {
                  account_key: claudeLogin.id,
                }),
              );
            else await state.refresh();
          }}
        />
      )}
      {!managerOnly &&
        renderPicker &&
        renderPicker(selectAccount, !!pending || (pinned && !owner))}
      {!managerOnly && renderPicker && teamTransfer?.status === "pending" && (
        <AccountTransferStatus
          transfer={teamTransfer}
          targetLabel={transferTarget?.email || transferTarget?.label}
          pending={!!pending}
          onAction={(kind) => {
            const requestId = teamTransfer.id;
            if (typeof requestId !== "string") return;
            void action(`${kind}-transfer`, () =>
              post("/api/agents/account-transfer", {
                action: kind,
                request_id: requestId,
              }),
            );
          }}
        />
      )}
      {!managerOnly && !renderPicker && (
        <Menu
          position="bottom-end"
          width={300}
          withinPortal
          onChange={setMenuOpened}
        >
          <Menu.Target>
            <Button
              className="account-picker"
              data-testid="account-picker"
              variant="subtle"
              aria-label={`Account: ${title}`}
              title={title}
              leftSection={<UserRound size={15} />}
              rightSection={<ChevronDown size={13} />}
            >
              <span className="account-picker-label">{title}</span>
              {transferring && (
                <span aria-label="Account transfer in progress">
                  {teamTransfer?.moved ?? teamTransfer?.completed}/
                  {teamTransfer?.total}
                </span>
              )}
            </Button>
          </Menu.Target>
          <Menu.Dropdown>
            <Menu.Label>
              {pinned
                ? "Transfer chat to account"
                : "Account for this conversation"}
            </Menu.Label>
            {accounts
              .filter(
                (account) => !account.disconnected || account.id === accountKey,
              )
              .sort(
                (a, b) =>
                  Number(projectAccountKeys?.includes(b.id) || false) -
                  Number(projectAccountKeys?.includes(a.id) || false),
              )
              .map((account) => (
                <Menu.Item
                  key={account.id}
                  disabled={
                    !!pending ||
                    account.status !== "ready" ||
                    !!account.disconnected ||
                    (pinned && !owner) ||
                    (transferring &&
                      account.id !== teamTransfer?.targetAccountKey)
                  }
                  leftSection={
                    account.id === accountKey ? (
                      <Check size={14} />
                    ) : (
                      <span style={{ width: 14 }} />
                    )
                  }
                  onClick={() => selectAccount(account.id)}
                >
                  <span className="account-menu-identity">
                    {account.email || account.label}
                    <small>
                      {[
                        projectAccountKeys?.includes(account.id)
                          ? "Project"
                          : null,
                        account.provider === "claude" ? "Claude Code" : null,
                        account.plan,
                        account.status !== "ready" ? account.status : null,
                      ]
                        .filter(Boolean)
                        .join(" · ") || "Ready"}
                      {account.id === state.data.defaultAccountKey
                        ? " · Application default"
                        : ""}
                    </small>
                    <AccountCapacity
                      account={account}
                      opened={menuOpened}
                      compact
                    />
                  </span>
                </Menu.Item>
              ))}
            <AccountTransferStatus
              transfer={teamTransfer ?? undefined}
              targetLabel={transferTarget?.email || transferTarget?.label}
              pending={!!pending}
              onAction={(kind) => {
                const requestId = teamTransfer?.id;
                if (typeof requestId !== "string") return;
                void action(`${kind}-transfer`, () =>
                  post("/api/agents/account-transfer", {
                    action: kind,
                    request_id: requestId,
                  }),
                );
              }}
            />
            <Menu.Divider />
            <Menu.Item
              leftSection={<Plus size={14} />}
              onClick={() => {
                setAddIntent(null);
                setAddOpen(true);
              }}
            >
              Add account
            </Menu.Item>
            <Menu.Item
              leftSection={<UserRound size={14} />}
              onClick={() => {
                setAdding(false);
                setOpened(true);
              }}
            >
              Manage accounts{accounts.length ? ` · ${accounts.length}` : ""}
            </Menu.Item>
            {Boolean(error || selected?.error) && (
              <ErrorDescription
                className="account-action-error"
                value={error || selected?.error}
              />
            )}
          </Menu.Dropdown>
        </Menu>
      )}
      {!dialogsOnly && (
        <AccountManagerHost
          inline={managerOnly}
          opened={opened}
          onClose={() => setOpened(false)}
          title="Accounts"
          closeBlocked={!!disconnectChoice || !!deleteChoice || !!claudeLogin}
        >
          {adding ? (
            <>
              <div className="accounts-add-header">
                <h3>Add account</h3>
                <Button variant="subtle" onClick={() => setAdding(false)}>
                  Back to accounts
                </Button>
              </div>
              <AccountSignIn state={state} />
            </>
          ) : (
            <div className="accounts-setup">
              {managerOnly && <h3>Accounts</h3>}
              {!accounts.length && (
                <p className="accounts-intro">
                  No accounts found. Sign in or find profiles on this computer.
                </p>
              )}
              <div className="accounts-actions">
                <Button
                  variant="filled"
                  color="indigo"
                  onClick={() => {
                    setAddIntent(null);
                    setAddOpen(true);
                  }}
                  leftSection={<Plus size={14} />}
                >
                  Add account
                </Button>
                <Button
                  variant="default"
                  leftSection={<RefreshCw size={14} />}
                  loading={pending === "discover"}
                  disabled={!!pending}
                  onClick={() =>
                    void action("discover", async () => {
                      state.setData(await post("/api/accounts/discover", {}));
                    })
                  }
                >
                  Find existing accounts
                </Button>
              </div>
            </div>
          )}
          {!adding && (
            <>
              <p className="accounts-intro">
                New chats use the application default unless the project has its
                own default. Existing chats keep their account.
              </p>
              <div className="accounts-list" aria-label="Saved accounts">
                {(["codex", "claude"] as const).map((provider) => {
                  const providerAccounts = accounts.filter(
                    (account) => (account.provider || "codex") === provider,
                  );
                  return providerAccounts.length ? (
                    <section
                      className="accounts-provider"
                      key={provider}
                      aria-label={`${provider === "claude" ? "Claude" : "Codex"} accounts`}
                    >
                      <h3>
                        <ProviderMark provider={provider} />
                        {provider === "claude" ? "Claude" : "Codex"}
                      </h3>
                      <div className="accounts-grid">
                        {providerAccounts.map((account) => (
                          <section
                            className="account-row"
                            key={account.id}
                            data-account={account.id}
                            aria-label={account.email || account.label}
                          >
                            <div className="account-card-heading">
                              <AccountNameEditor
                                account={account}
                                onSaved={state.setData}
                                disabled={!!pending}
                              />
                              <Menu position="bottom-end" withinPortal>
                                <Menu.Target>
                                  <ActionButton
                                    actionRole="quiet"
                                    className="account-card-menu"
                                    loading={
                                      pending === `default:${account.id}`
                                    }
                                    aria-label={`Actions for ${account.email || account.label}`}
                                  >
                                    <Ellipsis size={18} />
                                  </ActionButton>
                                </Menu.Target>
                                <Menu.Dropdown>
                                  {account.id !==
                                    state.data.defaultAccountKey && (
                                    <Menu.Item
                                      disabled={
                                        !!pending ||
                                        account.status !== "ready" ||
                                        !!account.disconnected
                                      }
                                      aria-label={`Use ${account.email || account.label} by default`}
                                      onClick={() =>
                                        void action(
                                          `default:${account.id}`,
                                          async () => {
                                            state.setData(
                                              await post(
                                                "/api/accounts/default",
                                                {
                                                  account_key: account.id,
                                                },
                                              ),
                                            );
                                          },
                                        )
                                      }
                                    >
                                      Set application default
                                    </Menu.Item>
                                  )}
                                  {account.provider === "claude" && (
                                    <>
                                      <Menu.Item
                                        onClick={() => setClaudeLogin(account)}
                                      >
                                        Sign in again
                                      </Menu.Item>
                                    </>
                                  )}
                                  {account.provider !== "claude" &&
                                    account.accountId && (
                                      <Menu.Item
                                        onClick={() => setCodexLogin(account)}
                                      >
                                        Sign in again
                                      </Menu.Item>
                                    )}
                                  {(state.data.supportsDisconnect ||
                                    state.data.supportsDelete) && (
                                    <Menu.Divider />
                                  )}
                                  {state.data.supportsDisconnect && (
                                    <Menu.Item
                                      color={
                                        account.disconnected ? "gray" : "red"
                                      }
                                      className={
                                        account.disconnected
                                          ? undefined
                                          : "account-destructive-action"
                                      }
                                      disabled={!!pending}
                                      onClick={() => {
                                        if (account.disconnected)
                                          void action(
                                            `reconnect:${account.id}`,
                                            async () => {
                                              state.setData(
                                                await post(
                                                  "/api/accounts/reconnect",
                                                  {
                                                    account_key: account.id,
                                                  },
                                                ),
                                              );
                                            },
                                          );
                                        else setDisconnectChoice(account);
                                      }}
                                    >
                                      {account.disconnected
                                        ? "Reconnect account"
                                        : "Disconnect account"}
                                    </Menu.Item>
                                  )}
                                  {state.data.supportsDelete && (
                                    <Menu.Item
                                      color="red"
                                      className="account-destructive-action"
                                      disabled={!!pending}
                                      onClick={() => {
                                        const requestId =
                                          storedDeleteRequest(
                                            state.scope,
                                            account.id,
                                          ) || crypto.randomUUID();
                                        saveDeleteRequest(
                                          state.scope,
                                          account.id,
                                          requestId,
                                        );
                                        setDeleteRequestId(requestId);
                                        setDeleteChoice(account);
                                      }}
                                    >
                                      Delete account
                                    </Menu.Item>
                                  )}
                                </Menu.Dropdown>
                              </Menu>
                            </div>
                            {account.email && (
                              <small className="account-email">
                                {account.email}
                              </small>
                            )}
                            <div className="account-card-status">
                              <span
                                className="account-status"
                                data-status={
                                  account.error || account.status === "error"
                                    ? "error"
                                    : account.status === "ready"
                                      ? "ready"
                                      : "signedOut"
                                }
                              >
                                {account.error || account.status === "error"
                                  ? "Error"
                                  : account.status === "ready"
                                    ? "Signed in"
                                    : "Sign in needed"}
                              </span>
                              {account.plan && (
                                <span className="account-plan">
                                  {account.plan}
                                </span>
                              )}
                              {account.id === state.data.defaultAccountKey && (
                                <span className="account-default">
                                  <Check size={12} /> Application default
                                </span>
                              )}
                            </div>
                            {Boolean(account.error) && (
                              <ErrorDescription
                                className="account-action-error"
                                value={account.error}
                              />
                            )}
                            {account.authenticationRecovery && (
                              <p role="alert">
                                {account.authenticationRecovery}
                              </p>
                            )}
                            {account.disconnected && (
                              <p>Hidden from new chats.</p>
                            )}
                            {!account.disconnected && (
                              <AccountCapacity
                                card
                                account={account}
                                opened={managerOnly || opened}
                              />
                            )}
                            {account.provider === "claude" && (
                              <ClaudeProfile
                                account={account}
                                onSaved={state.setData}
                              />
                            )}
                          </section>
                        ))}
                      </div>
                    </section>
                  ) : null;
                })}
              </div>
              <NativeRuntimeStatus
                opened={managerOnly || opened}
                accounts={accounts}
              />
            </>
          )}
          {adding && <ClaudeProfile onSaved={state.setData} />}
          {adding && (
            <details className="account-register">
              <summary>Add a Codex home directory</summary>
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  void action("register", async () => {
                    state.setData(
                      await post("/api/accounts/register", {
                        home: home.trim(),
                      }),
                    );
                    setHome("");
                  });
                }}
              >
                <TextInput
                  label="Codex home"
                  description="Enter the path to the existing .codex directory for this account."
                  placeholder="/path/to/.codex"
                  value={home}
                  onChange={(e) => setHome(e.target.value)}
                  autoComplete="off"
                />
                <Button
                  type="submit"
                  size="compact-sm"
                  disabled={!home.trim() || !!pending}
                  loading={pending === "register"}
                >
                  Add profile
                </Button>
              </form>
            </details>
          )}
          {(error || state.error) && (
            <p className="account-action-error" role="alert">
              {error || state.error}
            </p>
          )}
        </AccountManagerHost>
      )}
      <AccountAddDialog
        opened={addOpen}
        intent={addIntent}
        state={state}
        onClose={() => setAddOpen(false)}
      />
      <Modal
        opened={!!renameChoice}
        onClose={() => setRenameChoice(null)}
        title={`Rename ${renameChoice?.email || renameChoice?.label || "account"}`}
        centered
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (!renameChoice) return;
            void action(`rename:${renameChoice.id}`, async () => {
              state.setData(
                await post("/api/accounts/name", {
                  account_key: renameChoice.id,
                  label: renameLabel,
                  request_id: crypto.randomUUID(),
                }),
              );
              setRenameChoice(null);
            });
          }}
        >
          <Stack gap="sm">
            <TextInput
              label="Name"
              value={renameLabel}
              maxLength={32}
              onChange={(event) => setRenameLabel(event.currentTarget.value)}
            />
            <Group justify="flex-end">
              <Button variant="default" onClick={() => setRenameChoice(null)}>
                Cancel
              </Button>
              <Button
                type="submit"
                color="indigo"
                disabled={!renameLabel.trim() || !!pending}
                loading={pending === `rename:${renameChoice?.id}`}
              >
                Save
              </Button>
            </Group>
          </Stack>
        </form>
      </Modal>
      <AccountTransferConfirmation
        opened={!!transferChoice}
        target={transferChoice?.target || null}
        sourceProvider={transferChoice?.sourceProvider || "codex"}
        extend={transferChoice?.extend}
        onClose={() => setTransferChoice(null)}
        onConfirm={async () => {
          if (!transferChoice) return;
          await post("/api/agents/account-transfer", {
            id: transferChoice.agentId,
            account_key: transferChoice.target.id,
            request_id: transferChoice.requestId,
          });
        }}
      />
      <Modal
        opened={!!disconnectChoice}
        onClose={() => {
          if (!pending) setDisconnectChoice(null);
        }}
        title="Disconnect account"
        closeOnClickOutside={!pending}
        closeOnEscape={!pending}
        withCloseButton={!pending}
      >
        <h3 className="account-disconnect-identity">
          {disconnectChoice?.email || disconnectChoice?.label}
        </h3>
        <ul className="account-disconnect-effects">
          <li>New chats cannot use this account.</li>
          <li>Existing chats keep it. You can reconnect it later.</li>
          <li>
            Projects with this default need another account before new chats can
            start.
          </li>
        </ul>
        {disconnectsDefault && (
          <p className="account-replacement">
            {replacement
              ? `The application default changes to ${replacement.email || replacement.label}.`
              : "Connect another account before disconnecting the application default."}
          </p>
        )}
        <div className="account-confirm-actions">
          <Button
            variant="default"
            disabled={!!pending}
            onClick={() => setDisconnectChoice(null)}
          >
            Cancel
          </Button>
          <Button
            variant="filled"
            color="red"
            loading={pending === "disconnect"}
            disabled={!!pending || (disconnectsDefault && !replacement)}
            onClick={() => {
              const accountKey = disconnectChoice?.id;
              if (!accountKey) return;
              void action("disconnect", async () => {
                state.setData(
                  await post("/api/accounts/disconnect", {
                    account_key: accountKey,
                  }),
                );
                setDisconnectChoice(null);
              });
            }}
          >
            Disconnect account
          </Button>
        </div>
        {error && <p role="alert">{error}</p>}
      </Modal>
      <Modal
        opened={!!deleteChoice}
        onClose={() => {
          if (!pending) {
            setDeleteChoice(null);
          }
        }}
        title="Delete account"
        closeOnClickOutside={!pending}
        closeOnEscape={!pending}
        withCloseButton={!pending}
      >
        <p>
          Delete <strong>{deleteChoice?.email || deleteChoice?.label}</strong>{" "}
          from this account list?
        </p>
        <p>
          New chats and account choices will no longer use it. Existing chats
          and active work keep their account identity. Native credentials and
          saved history are preserved.
        </p>
        {deletesDefault && (
          <p>
            {deleteReplacement
              ? `The application default changes to ${deleteReplacement.email || deleteReplacement.label}.`
              : "Add another connected account before deleting the application default."}
          </p>
        )}
        <Button
          variant="default"
          disabled={!!pending}
          onClick={() => {
            setDeleteChoice(null);
          }}
        >
          Cancel
        </Button>
        <Button
          color="red"
          loading={pending === "delete"}
          disabled={!!pending || (deletesDefault && !deleteReplacement)}
          onClick={() => {
            if (!deleteChoice || !deleteRequestId) return;
            void action("delete", async () => {
              state.setData(
                await post("/api/accounts/delete", {
                  account_key: deleteChoice.id,
                  request_id: deleteRequestId,
                }),
              );
              saveDeleteRequest(state.scope, deleteChoice.id, null);
              setDeleteChoice(null);
              setDeleteRequestId("");
            });
          }}
        >
          Delete account
        </Button>
        {error && <p role="alert">{error}</p>}
      </Modal>
    </>
  );
}
