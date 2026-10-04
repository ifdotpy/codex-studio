import { localDateTime } from "../local-time";
import ErrorDescription from "./ErrorDescription";
import ClaudeProfile from "./ClaudeProfile";
import ClaudeSignIn from "./ClaudeSignIn";
import CodexSignIn from "./CodexSignIn";
import NativeRuntimeStatus from "./NativeRuntimeStatus";
import { accountLimits } from "../usage/accountUsage";
import { Button, Menu, Modal, TextInput } from "@mantine/core";
import {
  Check,
  ChevronDown,
  ArrowRightLeft,
  Plus,
  RefreshCw,
  UserRound,
} from "lucide-react";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type SetStateAction,
} from "react";
import { errorText, get, post, type GetResult } from "../api";
import type { Agent } from "../types";
import "./accounts.css";
import AccountSignIn from "./AccountSignIn";
import AccountManagerHost from "./AccountManagerHost";
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
    void refresh();
    const timer = window.setInterval(() => void refresh(), 30000);
    return () => {
      latestRead.current++;
      window.clearInterval(timer);
    };
  }, [stateDir, refresh]);
  return { data, setData, error, refresh, scope: stateDir };
}

function AccountCapacity({
  account,
  opened,
  compact = false,
}: {
  account: Account;
  opened: boolean;
  compact?: boolean;
}) {
  const [limits, setLimits] = useState<GetResult<"/api/limits"> | null>(null);
  const [limitsError, setLimitsError] = useState<unknown>(null);
  useEffect(() => {
    if (!opened || account.status !== "ready") return;
    let live = true;
    setLimits(null);
    setLimitsError(null);
    void get("/api/limits", {
      query: { account_key: account.id },
      timeoutMs: 25000,
    })
      .then((result) => {
        if (!accountLimits(result, account.id, account.accountId))
          throw new Error("Limits belong to another account.");
        if (live) setLimits(result);
      })
      .catch((error) => {
        if (live) setLimitsError(error);
      });
    return () => {
      live = false;
    };
  }, [account.id, account.accountId, account.status, account.provider, opened]);
  if (account.status !== "ready") return null;
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
        : limitsError || limits?.error || !weekly || weekly.remaining === null
          ? "Weekly: unavailable"
          : weekly.expired
            ? "Weekly: awaiting update"
            : `Weekly: ${formatPercent(weekly.remaining)} left`;
    return <small className="account-weekly-limit">{label}</small>;
  }

  return (
    <div
      className="account-capacity"
      aria-label={`Limits for ${account.email || account.label}`}
    >
      {!limits && !limitsError && <small>Reading limits…</small>}
      {(limitsError || limits?.error) && (
        <ErrorDescription
          className="account-action-error"
          role="status"
          value={limitsError || limits?.error}
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
    !showCompleted
  )
    return null;
  return (
    <div className="account-menu-note" role="status">
      <ArrowRightLeft size={14} />
      <div>
        <div>
          {transfer.moved ?? transfer.completed ?? 0} moved to {targetLabel}
          {transfer.status === "pending" &&
            waiting > 0 &&
            ` · ${waiting} waiting`}
        </div>
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
}: {
  state: ReturnType<typeof useAccounts>;
  agent?: Agent;
  accountKey?: string;
  projectAccountKeys?: string[];
  onModalOpenChange?: (opened: boolean) => void;
  managerOnly?: boolean;
  changeAccount?: (key: string) => Promise<void>;
  onError: (message: string) => void;
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
  const actionLock = useRef(false);
  const childModalOpen =
    (!managerOnly && opened) ||
    !!transferChoice ||
    !!disconnectChoice ||
    !!deleteChoice ||
    !!claudeLogin ||
    !!codexLogin;
  useEffect(() => {
    onModalOpenChange?.(
      managerOnly
        ? !!disconnectChoice || !!deleteChoice || !!claudeLogin || !!codexLogin
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
    if (!opened && !managerOnly) return;
    void state.refresh();
  }, [opened, managerOnly, state.refresh]);
  return (
    <>
      {codexLogin && (
        <CodexSignIn
          account={codexLogin}
          state={state}
          onClose={() => setCodexLogin(null)}
        />
      )}
      {claudeLogin && (
        <ClaudeSignIn
          key={claudeLogin.id}
          account={claudeLogin}
          scope={state.scope}
          onClose={() => setClaudeLogin(null)}
          onReady={state.refresh}
        />
      )}
      {!managerOnly && (
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
                  onClick={() => {
                    const extendTransfer =
                      transferring &&
                      account.id === teamTransfer?.targetAccountKey;
                    if (account.id === accountKey && !extendTransfer) return;
                    if (pinned && owner)
                      setTransferChoice({
                        target: account,
                        sourceProvider:
                          selected?.provider || owner.provider || "codex",
                        agentId: owner.id,
                        requestId: crypto.randomUUID(),
                        extend: !!extendTransfer,
                      });
                    else if (!pinned)
                      void action(account.id, async () => {
                        await changeAccount?.(account.id);
                      });
                  }}
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
                setAdding(true);
                setOpened(true);
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
      <AccountManagerHost
        inline={managerOnly}
        opened={opened}
        onClose={() => setOpened(false)}
        title={adding ? "Add account" : "Accounts"}
        closeBlocked={!!disconnectChoice || !!deleteChoice || !!claudeLogin}
      >
        {adding ? (
          <AccountSignIn state={state} opened={managerOnly || opened} />
        ) : (
          <Button
            onClick={() => setAdding(true)}
            leftSection={<Plus size={14} />}
          >
            Add account
          </Button>
        )}
        {adding && (
          <Button variant="subtle" onClick={() => setAdding(false)}>
            Back to accounts
          </Button>
        )}
        {!adding && (
          <>
            <p className="accounts-intro">
              Used when a project has no default.
            </p>
            <NativeRuntimeStatus
              opened={managerOnly || opened}
              accounts={accounts}
            />
            <div className="accounts-list" aria-label="Saved accounts">
              {accounts.map((account) => (
                <section
                  className="account-row"
                  key={account.id}
                  data-account={account.id}
                  aria-label={account.email || account.label}
                >
                  <div className="account-row-header">
                    <div className="account-identity">
                      <strong>{account.email || account.label}</strong>
                      {account.plan && (
                        <span className="account-plan">{account.plan}</span>
                      )}
                      {account.status !== "ready" && (
                        <small>{account.status}</small>
                      )}
                    </div>
                    {account.id === state.data.defaultAccountKey ? (
                      <span className="account-default">
                        <Check size={12} /> Application default
                      </span>
                    ) : (
                      <Button
                        size="compact-xs"
                        variant="subtle"
                        disabled={
                          !!pending ||
                          account.status !== "ready" ||
                          !!account.disconnected
                        }
                        loading={pending === `default:${account.id}`}
                        aria-label={`Use ${account.email || account.label} by default`}
                        onClick={() =>
                          void action(`default:${account.id}`, async () => {
                            state.setData(
                              await post("/api/accounts/default", {
                                account_key: account.id,
                              }),
                            );
                          })
                        }
                      >
                        Set application default
                      </Button>
                    )}
                  </div>
                  {Boolean(account.error) && (
                    <ErrorDescription
                      className="account-action-error"
                      value={account.error}
                    />
                  )}
                  {account.authenticationRecovery && (
                    <p role="alert">{account.authenticationRecovery}</p>
                  )}
                  {account.disconnected && <p>Hidden from new chats.</p>}
                  {!account.disconnected && (
                    <AccountCapacity
                      account={account}
                      opened={managerOnly || opened}
                    />
                  )}
                  {account.provider === "claude" && (
                    <>
                      <Button
                        variant="subtle"
                        size="compact-xs"
                        onClick={() => setClaudeLogin(account)}
                      >
                        Sign in again
                      </Button>
                      <ClaudeProfile
                        account={account}
                        onSaved={state.setData}
                      />
                    </>
                  )}
                  {account.provider !== "claude" && account.accountId && (
                    <Button
                      variant="subtle"
                      size="compact-xs"
                      onClick={() => setCodexLogin(account)}
                    >
                      Sign in again
                    </Button>
                  )}
                  {state.data.supportsDisconnect && (
                    <Button
                      variant="subtle"
                      size="compact-xs"
                      disabled={!!pending}
                      onClick={() => {
                        if (account.disconnected)
                          void action(`reconnect:${account.id}`, async () => {
                            state.setData(
                              await post("/api/accounts/reconnect", {
                                account_key: account.id,
                              }),
                            );
                          });
                        else setDisconnectChoice(account);
                      }}
                    >
                      {account.disconnected
                        ? "Reconnect account"
                        : "Disconnect account"}
                    </Button>
                  )}
                  {state.data.supportsDelete && (
                    <Button
                      variant="subtle"
                      size="compact-xs"
                      disabled={!!pending}
                      onClick={() => {
                        const requestId =
                          storedDeleteRequest(state.scope, account.id) ||
                          crypto.randomUUID();
                        saveDeleteRequest(state.scope, account.id, requestId);
                        setDeleteRequestId(requestId);
                        setDeleteChoice(account);
                      }}
                    >
                      Delete account
                    </Button>
                  )}
                </section>
              ))}
            </div>
            {!accounts.length && (
              <p className="accounts-intro">
                No accounts found. Sign in or find profiles on this computer.
              </p>
            )}
            <div className="accounts-actions">
              <Button
                variant="subtle"
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
        <p>
          Remove{" "}
          <strong>{disconnectChoice?.email || disconnectChoice?.label}</strong>{" "}
          from new chat choices?
        </p>
        <p>
          New chats cannot use it. Existing chats keep it. You can reconnect it
          later.
        </p>
        {disconnectsDefault && (
          <p>
            {replacement
              ? `The application default changes to ${replacement.email || replacement.label}.`
              : "Connect another account before disconnecting the application default."}
          </p>
        )}
        <p>
          Projects with this default need another account before new chats can
          start.
        </p>
        <Button
          variant="default"
          disabled={!!pending}
          onClick={() => setDisconnectChoice(null)}
        >
          Cancel
        </Button>
        <Button
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
