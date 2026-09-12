import ErrorDescription from "./ErrorDescription";
import { accountLimits } from "../accountUsage";
import { Button, Menu, Modal, TextInput } from "@mantine/core";
import {
  Check,
  ChevronDown,
  ArrowRightLeft,
  Plus,
  RefreshCw,
  UserRound,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { api, errorText } from "../api";
import type { Agent, Json } from "../types";
import "./accounts.css";
import AccountSignIn, { type LoginReceipt } from "./AccountSignIn";
import { readBuckets, formatPercent } from "./Usage";

export interface Account {
  id: string;
  label: string;
  email?: string | null;
  plan?: string | null;
  accountId?: string | null;
  source?: string;
  status: string;
  disconnected?: boolean;
  error?: unknown;
}
export interface AccountsState {
  accounts: Account[];
  defaultAccountKey: string;
  logins?: LoginReceipt[];
  supportsDisconnect?: boolean;
}
export function useAccounts(stateDir?: string) {
  const [data, setData] = useState<AccountsState>({
    accounts: [],
    defaultAccountKey: "default",
  });
  const [error, setError] = useState("");
  const refresh = useCallback(async () => {
    try {
      const result = await api<AccountsState>("/api/accounts");
      setData(result);
      setError("");
      return result;
    } catch (e) {
      setError(errorText(e));
      return null;
    }
  }, []);
  useEffect(() => {
    if (!stateDir) return;
    void refresh();
    const timer = window.setInterval(() => void refresh(), 30000);
    return () => window.clearInterval(timer);
  }, [stateDir, refresh]);
  return { data, setData, error, refresh, scope: stateDir };
}

function AccountCapacity({
  account,
  opened,
}: {
  account: Account;
  opened: boolean;
}) {
  const [limits, setLimits] = useState<Json | null>(null);
  useEffect(() => {
    if (!opened || account.status !== "ready") return;
    let live = true;
    setLimits(null);
    void api(
      `/api/limits?account_key=${encodeURIComponent(account.id)}`,
      undefined,
      { timeoutMs: 25000 },
    )
      .then((result) => {
        if (!accountLimits(result, account.id, account.accountId))
          throw new Error("Limits belong to another account.");
        if (live) setLimits(result);
      })
      .catch((error) => {
        if (live) setLimits({ error });
      });
    return () => {
      live = false;
    };
  }, [account.id, account.status, opened]);
  if (account.status !== "ready") return null;
  const buckets = readBuckets(limits, Date.now() / 1000);
  return (
    <div
      className="account-capacity"
      aria-label={`Limits for ${account.email || account.label}`}
    >
      {!limits && <small>Reading limits…</small>}
      {limits?.error && (
        <ErrorDescription
          className="account-action-error"
          role="status"
          value={limits.error}
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
                    title={new Date(window.reset * 1000).toLocaleString()}
                  >
                    {window.expired ? "Due " : "Resets "}
                    {new Date(window.reset * 1000).toLocaleString(undefined, {
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
      {limits && !limits.error && !buckets.length && (
        <small>Limits unavailable</small>
      )}
    </div>
  );
}

export function AccountTransferStatus({
  transfer,
  targetLabel,
  pending,
  onAction,
}: {
  transfer?: Json;
  targetLabel?: string;
  pending?: boolean;
  onAction: (action: "retry" | "cancel") => void;
}) {
  if (transfer?.status !== "pending") return null;
  return (
    <div className="account-menu-note" role="status">
      <ArrowRightLeft size={14} />
      <div>
        <div>
          {transfer.completed}/{transfer.total} transferred to {targetLabel}
        </div>
        {transfer.waiting && <small>{transfer.waiting}</small>}
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
          <Button
            size="compact-xs"
            variant="subtle"
            disabled={pending}
            onClick={() => onAction("cancel")}
          >
            Cancel remaining
          </Button>
        </div>
      </div>
    </div>
  );
}

export function AccountTransferConfirmation({
  opened,
  onClose,
  target,
  onConfirm,
}: {
  opened: boolean;
  onClose: () => void;
  target: Account | null;
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
      title="Transfer this team"
      closeOnClickOutside={!pending}
      closeOnEscape={!pending}
      withCloseButton={!pending}
    >
      <p>
        Transfer the main agent and all its subagents to{" "}
        <strong>{target?.email || target?.label}</strong>?
      </p>
      <p>
        Active agents finish their current work first. Each agent keeps its
        conversation history.
      </p>
      <p>
        This changes the account for this team. It does not change the project
        default.
      </p>
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
        Transfer team
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
}: {
  state: ReturnType<typeof useAccounts>;
  agent?: Agent;
  accountKey: string;
  projectAccountKeys?: string[];
  onModalOpenChange?: (opened: boolean) => void;
  changeAccount: (key: string) => Promise<void>;
  onError: (message: string) => void;
}) {
  const [opened, setOpened] = useState(false);
  const [pending, setPending] = useState("");
  const [error, setError] = useState("");
  const [home, setHome] = useState("");
  const [adding, setAdding] = useState(false);
  const [transferChoice, setTransferChoice] = useState<{
    target: Account;
    agentId: string;
    requestId: string;
  } | null>(null);
  const [disconnectChoice, setDisconnectChoice] = useState<Account | null>(
    null,
  );
  const actionLock = useRef(false);
  const childModalOpen = opened || !!transferChoice || !!disconnectChoice;
  useEffect(() => {
    onModalOpenChange?.(childModalOpen);
    return () => onModalOpenChange?.(false);
  }, [childModalOpen, onModalOpenChange]);
  const accounts = state.data.accounts || [];
  const selected = accounts.find((a) => a.id === accountKey);
  const pinned =
    !!agent &&
    (!agent.isLead || !agent.empty || !!agent.threadId || !!agent.inFlight);
  const owner = agent?.isLead ? agent : undefined;
  const transfer = owner?.accountTransfer;
  const transferring = transfer?.status === "pending";
  const transferTarget = accounts.find(
    (a) => a.id === transfer?.targetAccountKey,
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
    if (!opened) return;
    void state.refresh();
  }, [opened, state.refresh]);
  return (
    <>
      <Menu position="bottom-end" width={300} withinPortal>
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
                {transfer.completed}/{transfer.total}
              </span>
            )}
          </Button>
        </Menu.Target>
        <Menu.Dropdown>
          <Menu.Label>
            {pinned
              ? "Transfer team to account"
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
                  account.disconnected ||
                  (pinned && !owner) ||
                  transferring
                }
                leftSection={
                  account.id === accountKey ? (
                    <Check size={14} />
                  ) : (
                    <span style={{ width: 14 }} />
                  )
                }
                onClick={() => {
                  if (account.id === accountKey) return;
                  if (pinned && owner)
                    setTransferChoice({
                      target: account,
                      agentId: owner.id,
                      requestId: crypto.randomUUID(),
                    });
                  else if (!pinned)
                    void action(account.id, () => changeAccount(account.id));
                }}
              >
                <span className="account-menu-identity">
                  {account.email || account.label}
                  <small>
                    {[
                      projectAccountKeys?.includes(account.id)
                        ? "Project"
                        : null,
                      account.plan,
                      account.status !== "ready" ? account.status : null,
                    ]
                      .filter(Boolean)
                      .join(" · ") || "Ready"}
                    {account.id === state.data.defaultAccountKey
                      ? " · Application default"
                      : ""}
                  </small>
                </span>
              </Menu.Item>
            ))}
          <AccountTransferStatus
            transfer={transfer}
            targetLabel={transferTarget?.email || transferTarget?.label}
            pending={!!pending}
            onAction={(kind) =>
              void action(`${kind}-transfer`, () =>
                api("/api/agents/account-transfer", {
                  action: kind,
                  request_id: transfer.id,
                }),
              )
            }
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
      <Modal
        opened={opened}
        onClose={() => setOpened(false)}
        title={adding ? "Add account" : "Accounts"}
        closeOnEscape={!disconnectChoice}
        closeOnClickOutside={!disconnectChoice}
        size="lg"
        classNames={{ body: "accounts-manager" }}
      >
        {adding ? (
          <AccountSignIn state={state} opened={opened} />
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
              The application default applies when a project has no default.
              Existing chats keep their account.
            </p>
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
                          account.disconnected
                        }
                        loading={pending === `default:${account.id}`}
                        aria-label={`Use ${account.email || account.label} by default`}
                        onClick={() =>
                          void action(`default:${account.id}`, async () => {
                            state.setData(
                              await api<AccountsState>(
                                "/api/accounts/default",
                                {
                                  account_key: account.id,
                                },
                              ),
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
                  {account.disconnected && (
                    <p>
                      Disconnected from new chat choices. Existing chats keep
                      this account.
                    </p>
                  )}
                  {!account.disconnected && (
                    <AccountCapacity account={account} opened={opened} />
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
                              await api<AccountsState>(
                                "/api/accounts/reconnect",
                                { account_key: account.id },
                              ),
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
                    state.setData(
                      await api<AccountsState>("/api/accounts/discover", {}),
                    );
                  })
                }
              >
                Find existing accounts
              </Button>
            </div>
          </>
        )}
        {adding && (
          <details className="account-register">
            <summary>Add a Codex home directory</summary>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                void action("register", async () => {
                  state.setData(
                    await api<AccountsState>("/api/accounts/register", {
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
      </Modal>
      <AccountTransferConfirmation
        opened={!!transferChoice}
        target={transferChoice?.target || null}
        onClose={() => setTransferChoice(null)}
        onConfirm={async () => {
          if (!transferChoice) return;
          await api("/api/agents/account-transfer", {
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
          Existing chats continue with this account. This does not sign out of
          Codex or delete credentials. You can reconnect it here.
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
          onClick={() =>
            void action("disconnect", async () => {
              state.setData(
                await api<AccountsState>("/api/accounts/disconnect", {
                  account_key: disconnectChoice?.id,
                }),
              );
              setDisconnectChoice(null);
            })
          }
        >
          Disconnect account
        </Button>
        {error && <p role="alert">{error}</p>}
      </Modal>
    </>
  );
}
