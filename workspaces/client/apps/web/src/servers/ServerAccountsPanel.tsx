import {
  Accordion,
  Badge,
  Button,
  Menu,
  Modal,
  Select,
  Stack,
  TextInput,
} from "@mantine/core";
import { Check, CloudOff, Ellipsis, LogIn, RefreshCw } from "lucide-react";
import { useMemo, useRef, useState } from "react";
import {
  closeMenuFromTriggerOnEscape,
  restoreMenuTriggerFocusAfterClose,
  useMenuEscapeSuppression,
} from "../components/closeMenuFromTriggerOnEscape";
import { ActionButton, SettingsSection } from "../components/ui/primitives";
import type { ServerAccount, ServerCommand } from "./navigation";
import type { StudioServer } from "./registry";
import {
  rowServers,
  serverAccountRows,
  type ServerAccountRow,
} from "./serverAccountRows";
import {
  AccountLimitsAcrossServers,
  matchServerAccount,
  ServerTurnStartLimits,
  useServerFullAccounts,
  type AccountSource,
} from "./ServerAccountLimits";
import { AccountCapacityView, type Account } from "../components/Accounts";
import { accountLimitView, ProviderMark } from "../components/AccountTiles";
import type { ServerServices } from "./serverServices";

type AccountAction = Extract<ServerCommand, { action: "account-action" }>;
type StartAccount = (
  serverId: string,
  command: Extract<ServerCommand, { action: "account-sign-in" }>,
) => void;

function serverConnection(status?: string) {
  if (status === "offline") return { label: "Offline", blocked: true };
  if (status === "schema-mismatch")
    return { label: "Update required", blocked: true };
  if (status === "degraded")
    return { label: "Connection degraded", blocked: false };
  if (status === "connecting") return { label: "Connecting", blocked: false };
  if (status === "live") return null;
  return { label: "Connection unknown", blocked: false };
}

function signInStatus(status: string) {
  if (status === "ready") return "Signed in";
  if (status === "signedOut" || status === "changed") return "Sign in needed";
  if (status === "error") return "Sign-in check failed";
  return "Sign-in status unknown";
}

/** The copies of one provider account on each server that has it, with the
 * server's full account record. A disconnected copy is left out. */
export function accountSources(
  row: ServerAccountRow,
  servers: StudioServer[],
  fullAccounts: Record<string, Account[]>,
  servicesFor?: (serverId: string) => ServerServices | undefined,
): AccountSource[] {
  return servers.flatMap((server) => {
    const services = servicesFor?.(server.id);
    if (servicesFor && !services) return [];
    const listed = row.byServer[server.id];
    const account =
      listed && matchServerAccount(fullAccounts[server.id], listed);
    return account && !account.disconnected
      ? [
          {
            serverId: server.id,
            serverLabel: server.label,
            account,
            services,
          },
        ]
      : [];
  });
}

export default function ServerAccountsPanel({
  servers,
  accountsByServer,
  statuses,
  startAccount,
  accountAction,
  onDiscover,
  servicesFor,
  initialFocusIdentity,
  onInitialFocus,
}: {
  servers: StudioServer[];
  accountsByServer: Record<string, ServerAccount[]>;
  statuses: Record<string, string>;
  startAccount: StartAccount;
  accountAction: (serverId: string, command: AccountAction) => void;
  onDiscover?: () => void;
  /** The API of each server; without it the default API serves every server. */
  servicesFor?: (serverId: string) => ServerServices | undefined;
  initialFocusIdentity?: string;
  onInitialFocus?: () => void;
}) {
  const [fullAccounts, setFullAccounts] = useServerFullAccounts(
    servers.map((server) => server.id),
    servicesFor,
  );
  const [adding, setAdding] = useState(false);
  const [provider, setProvider] = useState<"codex" | "claude" | "">("");
  const [serverId, setServerId] = useState(servers[0]?.id || "local");
  const [label, setLabel] = useState("");
  const [email, setEmail] = useState("");
  const [openAccountMenuKey, setOpenAccountMenuKey] = useState<string | null>(
    null,
  );
  const accountMenuTriggers = useRef(new Map<string, HTMLButtonElement>());
  const { suppressNextEscape, clearSuppression } = useMenuEscapeSuppression(
    openAccountMenuKey !== null,
  );
  const rows = useMemo(
    () => serverAccountRows(servers, accountsByServer),
    [accountsByServer, servers],
  );
  const selected =
    servers.find((server) => server.id === serverId) || servers[0];
  return (
    <div className="server-accounts-panel" role="region" aria-label="Accounts">
      <SettingsSection
        title="Accounts"
        action={
          <div className="server-account-actions">
            {onDiscover && (
              <ActionButton
                actionRole="secondary"
                leftSection={<RefreshCw size={14} />}
                onClick={onDiscover}
              >
                Find existing accounts
              </ActionButton>
            )}
            <ActionButton
              actionRole="primary"
              onClick={() => {
                setProvider("");
                setAdding(true);
              }}
            >
              Add account
            </ActionButton>
          </div>
        }
      >
        {!rows.length ? (
          <p>No accounts found. Add a Codex or Claude account.</p>
        ) : (
          <Accordion
            className="server-accounts-list"
            keepMounted={false}
            transitionDuration={0}
            chevronPosition="left"
          >
            {rows.map((row) => {
              const sources = accountSources(
                row,
                servers,
                fullAccounts,
                servicesFor,
              );
              const exactAccountKey = (serverId: string) =>
                row.byServer[serverId]?.accountKey ||
                sources.find((source) => source.serverId === serverId)?.account
                  .id;
              const cannotSignIn = (server: StudioServer) =>
                serverConnection(statuses[server.id])?.blocked === true ||
                (!!servicesFor && !servicesFor(server.id)) ||
                (!!row.byServer[server.id] && !exactAccountKey(server.id));
              const signIn = (server: StudioServer) => {
                if (cannotSignIn(server)) return;
                const accountKey = exactAccountKey(server.id);
                startAccount(server.id, {
                  action: "account-sign-in",
                  ...(accountKey ? { accountKey } : {}),
                  provider: row.provider,
                  email: row.email,
                  label: row.label,
                  serverLabel: server.label,
                });
              };
              const facts = (
                <>
                  <dl className="account-facts">
                    <dt>Email</dt>
                    <dd>{row.email || "Unavailable"}</dd>
                    <dt>Servers</dt>
                    <dd>
                      {servers
                        .filter((server) => row.byServer[server.id])
                        .map((server) => server.label)
                        .join(", ")}
                    </dd>
                    <dt>Default</dt>
                    <dd>
                      {servers
                        .filter((server) => row.byServer[server.id]?.isDefault)
                        .map((server) => server.label)
                        .join(", ") || "No"}
                    </dd>
                    <dt>Sign-in</dt>
                    <dd>
                      {servers
                        .filter((server) => row.byServer[server.id])
                        .map(
                          (server) =>
                            `${server.label}: ${signInStatus(row.byServer[server.id]!.status)}`,
                        )
                        .join(", ")}
                    </dd>
                  </dl>
                  <small className="server-accounts-help">
                    Limits are shared across servers. Sign-in and turn start
                    limits are set per server.
                  </small>
                </>
              );
              const accountDetails = (
                <>
                  <ul
                    className="server-account-servers"
                    aria-label={`Servers for ${row.email || row.label}`}
                  >
                    {rowServers(row, servers).map((server) => {
                      const account = row.byServer[server.id];
                      const connection = serverConnection(statuses[server.id]);
                      const blocked = cannotSignIn(server);
                      const state = blocked
                        ? "offline"
                        : account?.status === "ready"
                          ? "ready"
                          : account?.status === "error"
                            ? "error"
                            : account
                              ? "reauth"
                              : "missing";
                      const text =
                        state === "ready"
                          ? `✓ ${server.label}`
                          : state === "error"
                            ? `⚠ ${server.label} · sign-in check failed`
                            : state === "reauth"
                              ? `⚠ ${server.label} · ${signInStatus(account!.status).toLocaleLowerCase()}`
                              : state === "offline"
                                ? `${server.label} · ${connection?.label.toLocaleLowerCase() || "connection unknown"}`
                                : `+ ${server.label}`;
                      const source = sources.find(
                        (item) => item.serverId === server.id,
                      );
                      return (
                        <li
                          key={server.id}
                          className="server-account-server"
                          data-server-id={server.id}
                        >
                          <button
                            type="button"
                            className="server-account-chip"
                            data-state={state}
                            disabled={blocked}
                            onClick={() => signIn(server)}
                          >
                            {text}
                          </button>
                          {source && (
                            <ServerTurnStartLimits
                              source={source}
                              onSaved={(accounts) =>
                                setFullAccounts((old) => ({
                                  ...old,
                                  [server.id]: accounts,
                                }))
                              }
                            />
                          )}
                        </li>
                      );
                    })}
                  </ul>
                </>
              );
              return (
                <Accordion.Item
                  value={row.key}
                  className="server-account-row"
                  key={row.key}
                  data-account-identity={row.key}
                >
                  <div className="server-account-heading">
                    <Accordion.Control
                      aria-label={`Limit details for ${row.email || row.label}`}
                    >
                      <span className="account-identity">
                        <span
                          role="img"
                          aria-label={
                            row.provider === "claude" ? "Claude" : "Codex"
                          }
                        >
                          <ProviderMark provider={row.provider} />
                        </span>
                        <strong title={row.label || row.email || "Account"}>
                          {row.label || row.email || "Account"}
                        </strong>
                      </span>
                    </Accordion.Control>
                    {row.plan && (
                      <span className="account-plan">{row.plan}</span>
                    )}
                    <span className="account-status-markers">
                      {servers
                        .filter((server) => row.byServer[server.id])
                        .map((server) => {
                          const connection = serverConnection(
                            servicesFor && !servicesFor(server.id)
                              ? undefined
                              : statuses[server.id],
                          );
                          if (!connection) return null;
                          const label = `${server.label}: ${connection.label}`;
                          return (
                            <span
                              key={server.id}
                              className="account-strip-state"
                              title={label}
                            >
                              <CloudOff size={14} aria-hidden="true" />
                              <span>{label}</span>
                            </span>
                          );
                        })}
                      {servers
                        .filter(
                          (server) =>
                            row.byServer[server.id]?.status !== "ready" &&
                            row.byServer[server.id],
                        )
                        .map((server) => (
                          <span
                            key={server.id}
                            className="account-strip-state"
                            title={`${server.label}: ${signInStatus(row.byServer[server.id]!.status)}`}
                          >
                            <LogIn size={14} aria-hidden="true" />
                            <span>
                              {server.label}:{" "}
                              <span>
                                {signInStatus(row.byServer[server.id]!.status)}
                              </span>
                            </span>
                          </span>
                        ))}

                      {servers
                        .filter((server) => row.byServer[server.id]?.isDefault)
                        .map((server) => (
                          <Badge
                            key={server.id}
                            size="xs"
                            color="blue"
                            variant="light"
                            title={`Default ${server.label}`}
                          >
                            <Check size={12} aria-hidden="true" />
                            <span className="account-strip-state-label">
                              Default {server.label}
                            </span>
                          </Badge>
                        ))}
                    </span>
                    <Menu
                      opened={openAccountMenuKey === row.key}
                      onChange={(nextOpened) => {
                        setOpenAccountMenuKey((current) =>
                          nextOpened
                            ? row.key
                            : current === row.key
                              ? null
                              : current,
                        );
                        if (!nextOpened)
                          restoreMenuTriggerFocusAfterClose(
                            accountMenuTriggers.current.get(row.key) ?? null,
                          );
                      }}
                      position="bottom-end"
                      withinPortal={false}
                      withInitialFocusPlaceholder={false}
                    >
                      <Menu.Target>
                        <ActionButton
                          className="account-card-menu"
                          data-autofocus={
                            initialFocusIdentity === row.key ? true : undefined
                          }
                          onFocus={() => {
                            if (initialFocusIdentity === row.key)
                              onInitialFocus?.();
                          }}
                          ref={(node) => {
                            if (node)
                              accountMenuTriggers.current.set(row.key, node);
                            else accountMenuTriggers.current.delete(row.key);
                          }}
                          actionRole="quiet"
                          onClick={(event) =>
                            event.currentTarget.focus({ preventScroll: true })
                          }
                          data-mantine-stop-propagation={
                            openAccountMenuKey === row.key || suppressNextEscape
                              ? "true"
                              : undefined
                          }
                          onKeyDown={(event) =>
                            closeMenuFromTriggerOnEscape(
                              event,
                              openAccountMenuKey === row.key,
                              () =>
                                setOpenAccountMenuKey((current) =>
                                  current === row.key ? null : current,
                                ),
                              suppressNextEscape,
                              clearSuppression,
                            )
                          }
                          onBlur={clearSuppression}
                          aria-label={`Actions for ${row.email || row.label}`}
                        >
                          <Ellipsis size={18} />
                        </ActionButton>
                      </Menu.Target>
                      <Menu.Dropdown data-mantine-stop-propagation="true">
                        {servers
                          .filter((server) => row.byServer[server.id])
                          .map((server) => (
                            <div key={server.id}>
                              <Menu.Label>{server.label}</Menu.Label>
                              {(
                                [
                                  ...(row.provider === "claude"
                                    ? (["configure-profile"] as const)
                                    : []),
                                  "rename",
                                  "default",
                                  "disconnect",
                                  "remove",
                                ] as const
                              ).map((operation) => (
                                <Menu.Item
                                  key={operation}
                                  disabled={cannotSignIn(server)}
                                  color={
                                    operation === "remove" ||
                                    operation === "disconnect"
                                      ? "red"
                                      : undefined
                                  }
                                  onClick={() => {
                                    if (cannotSignIn(server)) return;
                                    const accountKey = exactAccountKey(
                                      server.id,
                                    );
                                    if (!accountKey) return;
                                    accountAction(server.id, {
                                      action: "account-action",
                                      accountKey,
                                      operation,
                                      provider: row.provider,
                                      email: row.email,
                                      label: row.label,
                                    });
                                  }}
                                >
                                  {operation === "rename"
                                    ? "Rename"
                                    : operation === "configure-profile"
                                      ? "Configure"
                                      : operation === "default"
                                        ? "Make default"
                                        : operation === "disconnect"
                                          ? "Sign out"
                                          : "Remove"}
                                </Menu.Item>
                              ))}
                            </div>
                          ))}
                      </Menu.Dropdown>
                    </Menu>
                    <div
                      className="server-account-login-actions"
                      role="group"
                      aria-label={`Sign in for ${row.email || row.label}`}
                    >
                      {servers
                        .filter((server) => row.byServer[server.id])
                        .map((server) => (
                          <ActionButton
                            key={server.id}
                            actionRole="secondary"
                            leftSection={<LogIn size={14} aria-hidden="true" />}
                            aria-label={`Sign in to ${row.label || row.email} on ${server.label}`}
                            disabled={cannotSignIn(server)}
                            onClick={() => signIn(server)}
                          >
                            Sign in on {server.label}
                          </ActionButton>
                        ))}
                      {servers
                        .filter(
                          (server) =>
                            row.byServer[server.id] &&
                            !exactAccountKey(server.id),
                        )
                        .map((server) => (
                          <span role="status" key={server.id}>
                            {server.label}: Account identity unknown.
                          </span>
                        ))}
                    </div>
                  </div>
                  {sources.length ? (
                    <AccountLimitsAcrossServers
                      sources={sources}
                      extraDetails={accountDetails}
                      facts={facts}
                    />
                  ) : (
                    <AccountCapacityView
                      card
                      account={{
                        status: "signedOut",
                        email: row.email,
                        label: row.label,
                      }}
                      view={accountLimitView(
                        { id: row.key, status: "signedOut" },
                        null,
                      )}
                      facts={facts}
                      extraDetails={accountDetails}
                    />
                  )}
                </Accordion.Item>
              );
            })}
          </Accordion>
        )}
      </SettingsSection>
      <Modal
        opened={adding}
        onClose={() => setAdding(false)}
        title="Add account"
        centered
      >
        <Stack gap="sm">
          <div
            className="account-provider-choices"
            role="group"
            aria-label="Account type"
          >
            <button
              type="button"
              className={`account-provider-choice${provider === "codex" ? " selected" : ""}`}
              aria-pressed={provider === "codex"}
              onClick={() => setProvider("codex")}
            >
              <strong>Codex</strong>
              <span>ChatGPT subscription, one-time code</span>
            </button>
            <button
              type="button"
              className={`account-provider-choice${provider === "claude" ? " selected" : ""}`}
              aria-pressed={provider === "claude"}
              onClick={() => setProvider("claude")}
            >
              <strong>Claude</strong>
              <span>Claude subscription, link + paste code</span>
            </button>
          </div>
          <Select
            label="Server"
            data={servers.map((server) => ({
              value: server.id,
              label: server.label,
            }))}
            value={selected?.id || null}
            onChange={(value) => value && setServerId(value)}
          />
          <TextInput
            label="Name"
            value={label}
            maxLength={32}
            onChange={(event) => setLabel(event.currentTarget.value)}
          />
          <TextInput
            label="Email (optional)"
            type="email"
            value={email}
            onChange={(event) => setEmail(event.currentTarget.value)}
            description="Studio checks that you sign in with this account."
          />
          <div className="account-add-dialog-actions">
            <Button variant="default" onClick={() => setAdding(false)}>
              Cancel
            </Button>
            <Button
              disabled={!provider || !selected}
              onClick={() => {
                if (!selected || !provider) return;
                startAccount(selected.id, {
                  action: "account-sign-in",
                  provider,
                  email: email.trim() || null,
                  label:
                    label.trim() ||
                    email.trim() ||
                    (provider === "claude" ? "Claude Code" : "Codex"),
                  serverLabel: selected.label,
                  forceAdd: true,
                });
                setAdding(false);
                setLabel("");
                setEmail("");
              }}
            >
              Continue
            </Button>
          </div>
        </Stack>
      </Modal>
    </div>
  );
}
