import {
  Badge,
  Button,
  Menu,
  Modal,
  Select,
  Stack,
  TextInput,
} from "@mantine/core";
import { Ellipsis, RefreshCw } from "lucide-react";
import { useMemo, useState } from "react";
import { ActionButton, SettingsSection } from "../components/ui/primitives";
import type { ServerAccount, ServerCommand } from "./navigation";
import type { StudioServer } from "./registry";

type AccountAction = Extract<ServerCommand, { action: "account-action" }>;
type StartAccount = (
  serverId: string,
  command: Extract<ServerCommand, { action: "account-sign-in" }>,
) => void;

export default function ServerAccountsPanel({
  servers,
  accountsByServer,
  statuses,
  startAccount,
  accountAction,
  onDiscover,
}: {
  servers: StudioServer[];
  accountsByServer: Record<string, ServerAccount[]>;
  statuses: Record<string, string>;
  startAccount: StartAccount;
  accountAction: (serverId: string, command: AccountAction) => void;
  onDiscover?: () => void;
}) {
  const [adding, setAdding] = useState(false);
  const [provider, setProvider] = useState<"codex" | "claude" | "">("");
  const [serverId, setServerId] = useState(servers[0]?.id || "local");
  const [label, setLabel] = useState("");
  const [email, setEmail] = useState("");
  const rows = useMemo(() => {
    const grouped = new Map<
      string,
      {
        provider: "codex" | "claude";
        email: string | null;
        label: string;
        plan: string | null;
        byServer: Record<string, ServerAccount>;
      }
    >();
    for (const server of servers) {
      for (const account of accountsByServer[server.id] || []) {
        const key = `${account.provider}:${(account.email || account.label).toLocaleLowerCase()}`;
        const row = grouped.get(key) || {
          provider: account.provider,
          email: account.email,
          label: account.label,
          plan: account.plan,
          byServer: {},
        };
        row.byServer[server.id] = account;
        if (!row.email) row.email = account.email;
        if (!row.plan) row.plan = account.plan;
        grouped.set(key, row);
      }
    }
    return [...grouped.entries()].map(([key, account]) => ({
      key,
      ...account,
    }));
  }, [accountsByServer, servers]);
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
          <div className="server-accounts-list">
            {rows.map((row) => (
              <article
                className="server-account-row"
                key={row.key}
                data-account-identity={row.key}
              >
                <div className="server-account-heading">
                  <div>
                    <Badge size="xs" variant="light">
                      {row.provider === "claude" ? "Claude" : "Codex"}
                    </Badge>
                    <strong>{row.label || row.email || "Account"}</strong>
                    {servers
                      .filter((server) => row.byServer[server.id]?.isDefault)
                      .map((server) => (
                        <Badge
                          key={server.id}
                          size="xs"
                          color="blue"
                          variant="light"
                        >
                          Default · {server.label}
                        </Badge>
                      ))}
                    {!row.email && (
                      <span className="account-muted">Email unavailable</span>
                    )}
                    {row.plan && (
                      <small>
                        {row.email ? `${row.email} · ` : ""}
                        {row.plan}
                      </small>
                    )}
                    {row.email && !row.plan && <small>{row.email}</small>}
                  </div>
                  <Menu position="bottom-end" withinPortal>
                    <Menu.Target>
                      <ActionButton
                        actionRole="quiet"
                        aria-label={`Actions for ${row.email || row.label}`}
                      >
                        <Ellipsis size={18} />
                      </ActionButton>
                    </Menu.Target>
                    <Menu.Dropdown>
                      {servers
                        .filter((server) => row.byServer[server.id])
                        .map((server) => (
                          <div key={server.id}>
                            <Menu.Label>{server.label}</Menu.Label>
                            {(
                              [
                                "rename",
                                "default",
                                "disconnect",
                                "remove",
                              ] as const
                            ).map((operation) => (
                              <Menu.Item
                                key={operation}
                                color={
                                  operation === "remove" ||
                                  operation === "disconnect"
                                    ? "red"
                                    : undefined
                                }
                                onClick={() =>
                                  accountAction(server.id, {
                                    action: "account-action",
                                    operation,
                                    provider: row.provider,
                                    email: row.email,
                                    label: row.label,
                                  })
                                }
                              >
                                {operation === "rename"
                                  ? "Rename"
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
                </div>
                <div
                  className="server-account-chips"
                  aria-label={`Servers for ${row.email || row.label}`}
                >
                  {servers.map((server) => {
                    const account = row.byServer[server.id];
                    const offline = [
                      "offline",
                      "degraded",
                      "schema-mismatch",
                    ].includes(statuses[server.id] || "");
                    const state = offline
                      ? "offline"
                      : account?.status === "ready"
                        ? "ready"
                        : account
                          ? "reauth"
                          : "missing";
                    const text =
                      state === "ready"
                        ? `✓ ${server.label}`
                        : state === "reauth"
                          ? `⚠ ${server.label} · sign in again`
                          : state === "offline"
                            ? `${server.label} · offline`
                            : `+ ${server.label}`;
                    return (
                      <button
                        type="button"
                        key={server.id}
                        className="server-account-chip"
                        data-state={state}
                        disabled={offline}
                        onClick={() =>
                          startAccount(server.id, {
                            action: "account-sign-in",
                            provider: row.provider,
                            email: row.email,
                            label: row.label,
                            serverLabel: server.label,
                          })
                        }
                      >
                        {text}
                      </button>
                    );
                  })}
                </div>
              </article>
            ))}
          </div>
        )}
        <p className="server-accounts-help">
          Green: signed in on that server. Yellow: sign in again. Dashed: click
          to sign in on that server.
        </p>
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
              color="indigo"
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
