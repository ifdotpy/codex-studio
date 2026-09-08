import { accountLimits } from "../accountUsage";
import { Button, Menu, Modal, Switch, TextInput } from "@mantine/core";
import {
  Check,
  ChevronDown,
  Copy,
  ExternalLink,
  LockKeyhole,
  Plus,
  RefreshCw,
  ShieldAlert,
  UserRound,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { api, errorText } from "../api";
import type { Agent, Json } from "../types";
import "./accounts.css";
import { readBuckets, formatPercent } from "./Usage";
import AccountProjectRules, { projectRuleSummary } from "./AccountProjectRules";

export interface Account {
  id: string;
  label: string;
  email?: string | null;
  plan?: string | null;
  accountId?: string | null;
  source?: string;
  status: string;
  error?: string | null;
  projectRules?: { allowedProjects: string[] | null; revision: number };
}
export interface AccountsState {
  accounts: Account[];
  defaultAccountKey: string;
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
  return { data, setData, error, refresh };
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
        if (live) setLimits({ error: errorText(error) });
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
        <small className="account-action-error" role="status">
          {limits.error}
        </small>
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

export default function Accounts({
  state,
  agent,
  accountKey,
  changeAccount,
  lead,
  teamBusy,
  changeRuleOverride,
  onError,
}: {
  state: ReturnType<typeof useAccounts>;
  agent?: Agent;
  accountKey: string;
  changeAccount: (key: string) => Promise<void>;
  lead?: Agent;
  teamBusy: boolean;
  changeRuleOverride: (enabled: boolean) => Promise<void>;
  onError: (message: string) => void;
}) {
  const [opened, setOpened] = useState(false);
  const [pending, setPending] = useState("");
  const [error, setError] = useState("");
  const [home, setHome] = useState("");
  const [login, setLogin] = useState<{
    accountKey: string;
    verificationUrl?: string;
    userCode?: string;
    status: string;
  } | null>(null);
  const [copied, setCopied] = useState(false);
  const loginRequest = useRef<string | null>(null);
  const actionLock = useRef(false);
  const accounts = state.data.accounts || [];
  const selected = accounts.find((a) => a.id === accountKey);
  const pinned =
    !!agent &&
    (!agent.isLead || !agent.empty || !!agent.threadId || !!agent.inFlight);
  const title = selected?.email || selected?.label || "Codex account";
  const skipped = !!lead?.dangerouslySkipAccountRules;
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
    if (!login || login.status !== "pending") return;
    const timer = window.setInterval(() => void state.refresh(), 3000);
    return () => window.clearInterval(timer);
  }, [opened, login?.accountKey, login?.status, state.refresh]);
  const loginAccount = accounts.find((a) => a.id === login?.accountKey);
  const loginReady = loginAccount?.status === "ready";
  const loginError = loginAccount?.error;
  useEffect(() => {
    if (loginReady || loginError)
      setLogin((old) =>
        old ? { ...old, status: loginReady ? "ready" : "failed" } : old,
      );
  }, [loginReady, loginError]);
  const safeUrl = (() => {
    try {
      const url = new URL(login?.verificationUrl || "");
      return url.protocol === "https:" && !url.username && !url.password
        ? url.href
        : null;
    } catch {
      return null;
    }
  })();
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
            leftSection={
              skipped ? (
                <ShieldAlert size={15} className="account-rules-warning" />
              ) : (
                <UserRound size={15} />
              )
            }
            rightSection={<ChevronDown size={13} />}
          >
            <span className="account-picker-label">{title}</span>
          </Button>
        </Menu.Target>
        <Menu.Dropdown>
          <Menu.Label>
            {pinned
              ? "This conversation’s account"
              : "Account for this conversation"}
          </Menu.Label>
          {accounts.map((account) => (
            <Menu.Item
              key={account.id}
              disabled={
                !!pending ||
                account.status !== "ready" ||
                (pinned && account.id !== accountKey)
              }
              leftSection={
                account.id === accountKey ? (
                  <Check size={14} />
                ) : (
                  <span style={{ width: 14 }} />
                )
              }
              onClick={() =>
                void action(account.id, () => changeAccount(account.id))
              }
            >
              <span className="account-menu-identity">
                {account.email || account.label}
                <small>
                  {[
                    account.plan,
                    account.status !== "ready" ? account.status : null,
                  ]
                    .filter(Boolean)
                    .join(" · ") || "Ready"}
                  {account.id === state.data.defaultAccountKey
                    ? " · Default"
                    : ""}
                </small>
                <small>{projectRuleSummary(account)}</small>
              </span>
            </Menu.Item>
          ))}
          {pinned && (
            <p className="account-menu-note">
              <LockKeyhole size={12} /> New conversations can use another
              account.
            </p>
          )}
          <Menu.Divider />
          {lead && (
            <div className="account-rule-override">
              <Switch
                label="Dangerously skip rules"
                color="orange"
                checked={skipped}
                disabled={teamBusy || !!pending || !lead.isLead}
                onChange={(event) => {
                  const checked = event.currentTarget.checked;
                  void action("override", () => changeRuleOverride(checked));
                }}
              />
              <p>For this team only. Codex permissions stay active.</p>
              {teamBusy && <p>Wait for active turns to finish.</p>}
            </div>
          )}
          <Menu.Item
            leftSection={<UserRound size={14} />}
            onClick={() => setOpened(true)}
          >
            Manage accounts{accounts.length ? ` · ${accounts.length}` : ""}
          </Menu.Item>
          {(error || selected?.error) && (
            <p className="account-action-error" role="alert">
              {error || selected?.error}
            </p>
          )}
        </Menu.Dropdown>
      </Menu>
      {skipped && (
        <Button
          className="account-rules-badge"
          size="compact-xs"
          color="orange"
          variant="light"
          leftSection={<ShieldAlert size={12} />}
          title="Dangerously skip rules is enabled for this team"
          onClick={() => setOpened(true)}
        >
          Rules off
        </Button>
      )}
      <Modal
        opened={opened}
        onClose={() => setOpened(false)}
        title="Accounts"
        size="lg"
        classNames={{ body: "accounts-manager" }}
      >
        {lead && (
          <div className="account-rule-override account-rule-override-panel">
            <Switch
              label="Dangerously skip rules"
              color="orange"
              checked={skipped}
              disabled={teamBusy || !!pending || !lead.isLead}
              onChange={(event) => {
                const checked = event.currentTarget.checked;
                void action("override", () => changeRuleOverride(checked));
              }}
            />
            <p>For this team only. Codex permissions stay active.</p>
            {teamBusy && <p>Wait for active turns to finish.</p>}
          </div>
        )}
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
                    <Check size={12} /> Default
                  </span>
                ) : (
                  <Button
                    size="compact-xs"
                    variant="subtle"
                    disabled={!!pending || account.status !== "ready"}
                    loading={pending === `default:${account.id}`}
                    aria-label={`Use ${account.email || account.label} by default`}
                    onClick={() =>
                      void action(`default:${account.id}`, async () => {
                        state.setData(
                          await api<AccountsState>("/api/accounts/default", {
                            account_key: account.id,
                          }),
                        );
                      })
                    }
                  >
                    Use by default
                  </Button>
                )}
              </div>
              {account.error && (
                <p className="account-action-error" role="alert">
                  {account.error}
                </p>
              )}
              <AccountCapacity account={account} opened={opened} />
              <AccountProjectRules account={account} saved={state.setData} />
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
            leftSection={<Plus size={15} />}
            loading={pending === "login"}
            disabled={
              !!pending ||
              (login?.status === "pending" && !loginReady && !loginError)
            }
            onClick={() =>
              void action("login", async () => {
                loginRequest.current ||= crypto.randomUUID();
                const result = await api<NonNullable<typeof login>>(
                  "/api/accounts/login",
                  { request_id: loginRequest.current },
                );
                setLogin(result);
                loginRequest.current = null;
                setCopied(false);
                await state.refresh();
              })
            }
          >
            Sign in to another account
          </Button>
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
        {login && (
          <section className="account-login" aria-label="Account sign-in">
            {loginReady ? (
              <p role="status">
                <Check size={16} /> Account connected.
              </p>
            ) : loginError ? (
              <p role="alert">{loginError}</p>
            ) : (
              <>
                <strong>Complete sign-in in your browser</strong>
                <p>Copy this code, then open the sign-in page.</p>
                {login.userCode && (
                  <div className="account-login-code">
                    <code>{login.userCode}</code>
                    <Button
                      size="compact-xs"
                      variant="subtle"
                      leftSection={<Copy size={13} />}
                      onClick={() =>
                        void action("copy", async () => {
                          await navigator.clipboard.writeText(login.userCode!);
                          setCopied(true);
                        })
                      }
                    >
                      {copied ? "Copied" : "Copy code"}
                    </Button>
                  </div>
                )}
                {safeUrl ? (
                  <Button
                    component="a"
                    href={safeUrl}
                    target="_blank"
                    rel="noopener noreferrer"
                    size="compact-sm"
                    rightSection={<ExternalLink size={13} />}
                  >
                    Open sign-in page
                  </Button>
                ) : (
                  <p role="alert">A secure sign-in link is unavailable.</p>
                )}
                <small role="status">Waiting for sign-in…</small>
              </>
            )}
          </section>
        )}
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
        {(error || state.error) && (
          <p className="account-action-error" role="alert">
            {error || state.error}
          </p>
        )}
      </Modal>
    </>
  );
}
