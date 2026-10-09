import { serverRows, relativeSeen, type ServerStatus } from "./serverRows";
import { localDateTime } from "../local-time";
import { ActionButton, SettingsSection } from "../components/ui/primitives";
import DiscoveryPanel from "./DiscoveryPanel";
import ServerAccessSettings from "./ServerAccessSettings";
import { setServerExcluded } from "./automaticAccessStore";
import type { useServerDiscovery } from "./useServerDiscovery";
import type { ResourceConnectionState } from "../sync/resourceEvents";
import { parseInvitation } from "./pairing";
import {
  Badge,
  Group,
  Modal,
  Text,
  Tooltip,
  TextInput,
  Textarea,
} from "@mantine/core";
import { useRef, useState } from "react";
import {
  readServers,
  writeServers,
  localServer,
  serveOrigin,
  type StudioServer,
} from "./registry";
import { LOCAL_ALIAS_KEY, defaultServerAlias } from "./serverAliases";
import ServerAliasEditor from "./ServerAliasEditor";
import { serverCredentialAdapter } from "./transport";
import type { ServerNavigation, ServerAccount } from "./navigation";
export default function ServerManager({
  servers,
  add,
  remove,
  discovery,
  localEnabled,
  statuses = {},
  lastSeen = {},
  navigation = {},
  accountsByServer = {},
}: {
  servers: StudioServer[];
  add: (server: StudioServer) => void;
  remove: (server: StudioServer) => void;
  discovery: ReturnType<typeof useServerDiscovery>;
  localEnabled: boolean;
  statuses?: Record<string, ResourceConnectionState>;
  lastSeen?: Record<string, number>;
  navigation?: Record<string, ServerNavigation>;
  accountsByServer?: Record<string, ServerAccount[]>;
}) {
  const [origin, setOrigin] = useState("");
  const [code, setCode] = useState("");
  const [label, setLabel] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [manual, setManual] = useState(false);
  const [revoke, setRevoke] = useState<{ id: string; label: string } | null>(
    null,
  );
  const colors: Record<ServerStatus, string> = {
    Active: "green",
    Online: "green",
    Paired: "blue",
    Discovered: "blue",
    Unreachable: "gray",
    Offline: "gray",
    Revoked: "red",
    "Update required": "orange",
  };
  const request = useRef<{ origin: string; code: string; id: string } | null>(
    null,
  );
  return (
    <div className="studio-settings-panel" role="region" aria-label="Servers">
      {localEnabled && (
        <DiscoveryPanel
          snapshot={discovery.snapshot}
          busy={discovery.busy}
          error={discovery.error}
          actions={discovery.actions}
        />
      )}
      {discovery.retry && (
        <ActionButton
          actionRole="secondary"
          onClick={discovery.retry}
          disabled={discovery.busy}
        >
          Retry saved request
        </ActionButton>
      )}
      <SettingsSection title="Servers">
        <div className="server-card-grid">
          {serverRows(servers, discovery.snapshot?.servers || [], statuses).map(
            ({ server, peer, registered, status }) => {
              const seen =
                peer?.lastSeen ??
                lastSeen[server.id] ??
                (server.id === "local" ? discovery.checkedAt : null);
              const name =
                server.id === "local" && discovery.snapshot?.localLabel
                  ? `${discovery.snapshot.localLabel} (this computer)`
                  : server.label;
              const summary = navigation[server.id];
              const accounts = accountsByServer[server.id] || [];
              const codexCount = accounts.filter(
                (account) => account.provider === "codex",
              ).length;
              const claudeCount = accounts.length - codexCount;
              const displayStatus =
                status === "Revoked"
                  ? "Revoked"
                  : status === "Discovered"
                    ? "Discovered"
                    : statuses[server.id] === "live" || server.id === "local"
                      ? "Active"
                      : "Unreachable";
              const system =
                summary?.system === "Darwin"
                  ? "macOS"
                  : summary?.system || "OS unavailable";
              const host = new URL(
                server.id === "local"
                  ? discovery.snapshot?.localOrigin || server.origin
                  : server.origin,
              ).host;
              return (
                <article
                  key={server.id}
                  data-settings-server={server.id}
                  className="server-settings-card"
                >
                  <header className="server-settings-card-header">
                    <div>
                      <h3>{name}</h3>
                      <Badge size="xs" color={colors[status]} variant="light">
                        {displayStatus}
                      </Badge>
                    </div>
                    <Group gap="xs" justify="flex-end">
                      {server.id !== "local" && registered && (
                        <ActionButton
                          actionRole="secondary"
                          size="xs"
                          aria-label="Remove from this UI"
                          disabled={busy}
                          onClick={() => {
                            setError("");
                            try {
                              setServerExcluded(server.id, true);
                            } catch (failure) {
                              setError((failure as Error).message);
                              return;
                            }
                            setBusy(true);
                            void serverCredentialAdapter()
                              .forget(registered)
                              .then(() => remove(registered))
                              .catch((failure) =>
                                setError(
                                  failure instanceof Error
                                    ? failure.message
                                    : String(failure),
                                ),
                              )
                              .finally(() => setBusy(false));
                          }}
                        >
                          Remove
                        </ActionButton>
                      )}
                      {!registered && peer?.paired && localEnabled && (
                        <ActionButton
                          actionRole="secondary"
                          size="xs"
                          disabled={busy || discovery.busy}
                          onClick={() => discovery.addAgain(peer.id)}
                        >
                          Add to this UI
                        </ActionButton>
                      )}
                      {peer?.paired && localEnabled && (
                        <ActionButton
                          actionRole="secondary"
                          color="red"
                          size="xs"
                          disabled={busy || discovery.busy}
                          onClick={() =>
                            setRevoke({ id: peer.id, label: peer.label })
                          }
                        >
                          Revoke
                        </ActionButton>
                      )}
                      {peer?.status === "revoked" && localEnabled && (
                        <ActionButton
                          actionRole="secondary"
                          size="xs"
                          disabled={busy || discovery.busy}
                          onClick={() => discovery.allowAgain(peer.id)}
                        >
                          Allow again
                        </ActionButton>
                      )}
                    </Group>
                  </header>
                  <dl className="server-settings-facts">
                    <div>
                      <dt>Host</dt>
                      <dd>
                        {system} · {host}
                      </dd>
                    </div>
                    <div>
                      <dt>Accounts</dt>
                      <dd>
                        Codex {codexCount} · Claude {claudeCount}
                      </dd>
                    </div>
                    <div>
                      <dt>Agents running</dt>
                      <dd>
                        {summary?.agentsRunning ?? (summary?.busy ? 1 : 0)}
                      </dd>
                    </div>
                    <div>
                      <dt>Seen</dt>
                      <dd>
                        {seen == null ? (
                          "Unavailable"
                        ) : (
                          <Tooltip label={localDateTime(new Date(seen * 1000))}>
                            <time
                              dateTime={new Date(seen * 1000).toISOString()}
                            >
                              {relativeSeen(seen)}
                            </time>
                          </Tooltip>
                        )}
                      </dd>
                    </div>
                  </dl>
                  <small>
                    {server.id === "local"
                      ? discovery.snapshot?.localOrigin || server.origin
                      : server.origin}
                  </small>
                  {(registered || server.id === "local" || peer?.paired) && (
                    <ServerAliasEditor
                      value={
                        discovery.snapshot?.aliases?.[server.id] ||
                        registered?.alias ||
                        (server.id === "local"
                          ? localServer().alias
                          : undefined) ||
                        defaultServerAlias(server)
                      }
                      used={[
                        ...new Set([
                          ...servers
                            .filter((row) => row.id !== server.id)
                            .map((row) => row.alias || defaultServerAlias(row)),
                          ...Object.entries(discovery.snapshot?.aliases || {})
                            .filter(
                              ([id]) =>
                                id !== server.id &&
                                (server.id !== "local" ||
                                  id !== discovery.snapshot?.localServerId),
                            )
                            .map(([, alias]) => alias),
                        ]),
                      ]}
                      disabled={busy || discovery.busy}
                      save={async (alias) => {
                        if (
                          localEnabled &&
                          (server.id === "local" || peer?.paired)
                        )
                          return discovery.setAlias(server.id, alias);
                        if (server.id === "local")
                          localStorage.setItem(LOCAL_ALIAS_KEY, alias);
                        writeServers(
                          readServers().map((row) =>
                            row.id === server.id ? { ...row, alias } : row,
                          ),
                        );
                        return true;
                      }}
                    />
                  )}
                </article>
              );
            },
          )}
        </div>
      </SettingsSection>
      <Text size="xs" c="dimmed">
        Removal affects this UI. Revoke a manual UI connection in that server's
        settings.
      </Text>
      {error && (
        <p className="studio-preferences-error" role="alert">
          {error}
        </p>
      )}
      <SettingsSection
        title="Manual pairing"
        action={
          <ActionButton
            actionRole="secondary"
            onClick={() => setManual(!manual)}
          >
            {manual ? "Hide manual pairing" : "Add with an invitation"}
          </ActionButton>
        }
      >
        {manual && (
          <>
            {servers.some((server) => server.id === "local") && (
              <ServerAccessSettings active />
            )}
            <p>
              Open the server settings on the other computer. Create a pairing
              invitation there.
            </p>
            <form
              onSubmit={(event) => {
                event.preventDefault();
                if (busy) return;
                setError("");
                let address: string;
                try {
                  address = serveOrigin(origin);
                } catch (failure) {
                  setError(String((failure as Error).message));
                  return;
                }
                if (!code.trim()) {
                  setError("Paste the pairing invitation.");
                  return;
                }
                try {
                  const invitation = parseInvitation(code.trim(), address);
                  const saved = JSON.parse(
                    localStorage.getItem("studio-pairing-attempt-v1") || "null",
                  );
                  if (
                    !request.current ||
                    request.current.origin !== address ||
                    request.current.code !== code.trim()
                  ) {
                    const id =
                      saved?.origin === address &&
                      saved?.inviteId === invitation.inviteId
                        ? saved.id
                        : crypto.randomUUID();
                    request.current = {
                      origin: address,
                      code: code.trim(),
                      id,
                    };
                    localStorage.setItem(
                      "studio-pairing-attempt-v1",
                      JSON.stringify({
                        origin: address,
                        inviteId: invitation.inviteId,
                        id,
                      }),
                    );
                  }
                } catch (failure) {
                  setError((failure as Error).message);
                  return;
                }
                const attempt = request.current;
                setBusy(true);
                void (async () => {
                  const server = await serverCredentialAdapter().pair(
                    address,
                    attempt.code,
                    attempt.id,
                  );
                  setServerExcluded(server.id, false);
                  add({ ...server, label: label.trim() || server.label });
                  setCode("");
                  setOrigin("");
                  setLabel("");
                  request.current = null;
                  localStorage.removeItem("studio-pairing-attempt-v1");
                })()
                  .catch((failure: unknown) =>
                    setError(
                      failure instanceof Error
                        ? failure.message
                        : String(failure),
                    ),
                  )
                  .finally(() => setBusy(false));
              }}
            >
              <TextInput
                label="Server address"
                value={origin}
                onChange={(event) => setOrigin(event.currentTarget.value)}
                required
                disabled={busy}
              />
              <Textarea
                label="Pairing invitation"
                value={code}
                onChange={(event) => setCode(event.currentTarget.value)}
                required
                disabled={busy}
                autoComplete="off"
              />
              <TextInput
                label="Server name"
                value={label}
                onChange={(event) => setLabel(event.currentTarget.value)}
                maxLength={128}
                disabled={busy}
              />
              <ActionButton type="submit" actionRole="primary" loading={busy}>
                Pair server
              </ActionButton>
            </form>
          </>
        )}
      </SettingsSection>
      <Modal
        opened={!!revoke}
        onClose={() => setRevoke(null)}
        title="Revoke server access"
        size="sm"
      >
        <p>
          Revoke {revoke?.label} on this server? Automatic discovery cannot
          restore revoked access.
        </p>
        <Group justify="flex-end">
          <ActionButton actionRole="secondary" onClick={() => setRevoke(null)}>
            Cancel
          </ActionButton>
          <ActionButton
            actionRole="destructive"
            onClick={() => {
              if (revoke) discovery.revoke(revoke.id);
              setRevoke(null);
            }}
          >
            Revoke
          </ActionButton>
        </Group>
      </Modal>
    </div>
  );
}
