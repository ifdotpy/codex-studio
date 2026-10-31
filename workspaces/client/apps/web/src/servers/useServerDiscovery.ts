import { randomUUID } from "../uuid";
import { useServerApi, useServerResourceReads } from "./ServerServicesContext";
import {
  capturePreferenceWrite,
  readPreferenceFields,
} from "../sync/uiPreferenceStore";
import { preferenceEvent } from "../sync/uiPreferenceMerge";
import { ServerManagementRequests } from "./managementRequests";
import { useEffect, useRef, useState } from "react";
import type { ResourceVersion } from "../sync/subscriptions/events";
import { errorText } from "../api";
import {
  discoverySnapshot,
  effectiveDiscoveryAliases,
  type DiscoverySnapshot,
} from "./discoveryModel";
import type { ServerAccessRequest } from "./accessContract";
import { AutomaticUiAccess } from "./automaticUiAccess";
import { automaticAccessStore } from "./automaticAccessStore";
import {
  readServers,
  setLocalServerDetails,
  type StudioServer,
} from "./registry";
import { serverCredentialAdapter } from "./transport";
import { refreshServerName } from "./serverNames";
export const DISCOVERY_REFRESH_EVENT = "studio-server-discovery-refresh";
export function useServerDiscovery(
  enabled: boolean,
  add: (server: StudioServer) => void,
  localAvailable = true,
  workspaceIds: Record<string, string | undefined> = {},
) {
  const { refreshSession, serverAccess } = useServerApi();
  const watchResourceReads = useServerResourceReads();

  const [snapshot, setSnapshot] = useState<DiscoverySnapshot | null>(null);
  const [, setPreferenceRevision] = useState(0);
  const [error, setError] = useState("");
  const [accessError, setAccessError] = useState("");
  const [busy, setBusy] = useState(false);
  const [checkedAt, setCheckedAt] = useState<number | null>(null);
  useEffect(() => {
    const changed = (event: Event) => {
      if (
        event instanceof CustomEvent &&
        typeof event.detail === "string" &&
        event.detail.startsWith("server-alias:")
      )
        setPreferenceRevision((revision) => revision + 1);
    };
    window.addEventListener(preferenceEvent, changed);
    return () => window.removeEventListener(preferenceEvent, changed);
  }, []);
  const addRef = useRef(add);
  addRef.current = add;
  const active = useRef(false);
  const attempts = useRef<ReturnType<typeof automaticAccessStore> | null>(null);
  if (!attempts.current)
    attempts.current = automaticAccessStore((attempt) =>
      serverCredentialAdapter().hasPairAttempt({
        requestId: attempt.pairRequestId,
        serverId: attempt.serverId,
        origin: attempt.origin,
        inviteId: attempt.invitation?.inviteId,
      }),
    );
  const controller = useRef<AutomaticUiAccess | null>(null);
  if (!controller.current)
    controller.current = new AutomaticUiAccess(attempts.current, {
      current: async () => {
        // Revalidate permission before/after automatic pairing. A cached stream
        // snapshot cannot authorize a remote credential change.
        const value = await serverAccess("GET");
        return discoverySnapshot(value);
      },
      existing: readServers,
      lock: async (key, run) => {
        if (!navigator.locks)
          throw new Error(
            "Automatic UI access needs a browser with Web Locks support.",
          );
        await navigator.locks.request(`studio-automatic-ui-access:${key}`, run);
      },
      add: (server) => {
        if (active.current) addRef.current(server);
      },
      pair: (...args) => serverCredentialAdapter().pair(...args),
      hasPairAttempt: (attempt) =>
        serverCredentialAdapter().hasPairAttempt(attempt),
      invite: async (server, requestId) => {
        const session = await refreshSession();
        const value = await serverAccess(
          "POST",
          {
            action: "ui_invite",
            serverId: server.id,
            requestId,
          },
          15000,
          session.token,
        );
        if (!("invitation" in value))
          throw new Error("The server invitation response is invalid.");
        return value.invitation;
      },
    });
  const [pending, setPending] = useState<ServerAccessRequest | null>(null);
  const busyRef = useRef(false);
  const management = new ServerManagementRequests(
    location.origin,
    localStorage,
    async (name, work) => {
      if (!navigator.locks)
        throw new Error(
          "Server settings need a browser with Web Locks support.",
        );
      await navigator.locks.request(name, work);
    },
    async (body) => {
      if (!enabled && body.action === "name" && body.serverId !== "local") {
        const server = readServers().find((row) => row.id === body.serverId);
        if (!server) throw new Error("Select a paired server.");
        const response = await serverCredentialAdapter().fetch(
          server,
          new Request(server.origin + "/api/multi-server", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ ...body, serverId: "local" }),
            signal: AbortSignal.timeout(15000),
          }),
        );
        if (!response.ok) {
          const value = await response.json();
          throw new Error(value.error || "The server refused the name.");
        }
        return;
      }
      const session = await refreshSession();
      if (body.action === "revoke") controller.current?.cancel(body.clientId);
      await serverAccess(
        "POST",
        body,
        body.action === "discover"
          ? 105000
          : body.action === "name"
            ? 40000
            : 15000,
        session.token,
      );
    },
  );
  const apply = async (state: Parameters<typeof discoverySnapshot>[0]) => {
    const next = discoverySnapshot(state);
    if (!active.current && !localAvailable) return;
    const alias = effectiveDiscoveryAliases(
      next?.aliases,
      workspaceIds,
      readPreferenceFields("user"),
    )?.local;
    if (next?.localLabel || alias)
      setLocalServerDetails({
        ...(next?.localLabel ? { label: next.localLabel } : {}),
        ...(alias ? { alias } : {}),
      });
    setSnapshot(next);
    setCheckedAt(Date.now() / 1000);
  };
  const effectiveSnapshot = snapshot
    ? {
        ...snapshot,
        aliases: effectiveDiscoveryAliases(
          snapshot.aliases,
          Object.fromEntries(
            ["local", ...readServers().map((server) => server.id)].map((id) => [
              id,
              workspaceIds[id],
            ]),
          ),
          readPreferenceFields("user"),
        ),
      }
    : null;
  useEffect(() => {
    const alias = effectiveSnapshot?.aliases?.local;
    if (alias) setLocalServerDetails({ alias });
  }, [effectiveSnapshot?.aliases?.local]);
  const load = (version?: ResourceVersion) => {
    const snapshot = version?.discoverySnapshot;
    return snapshot ? apply(snapshot) : Promise.resolve();
  };
  const loadRef = useRef(load);
  loadRef.current = load;
  useEffect(() => {
    if (!localAvailable) return;
    const watcher = watchResourceReads(
      { kind: "multi-server" },
      async (version) => {
        await loadRef.current(version);
        setError("");
      },
      (failure) => setError(errorText(failure)),
      { waitForBaseline: true },
    );
    const refresh = () => watcher.refresh();
    window.addEventListener(DISCOVERY_REFRESH_EVENT, refresh);
    return () => {
      window.removeEventListener(DISCOVERY_REFRESH_EVENT, refresh);
      watcher();
    };
  }, [localAvailable, watchResourceReads]);
  useEffect(() => {
    if (!localAvailable) return;
    active.current = true;
    return () => {
      active.current = false;
      controller.current?.stop();
    };
  }, [localAvailable]);
  const run = async (request: ServerAccessRequest) => {
    if ((!enabled && request.action !== "name") || busyRef.current)
      return false;
    busyRef.current = true;
    setBusy(true);
    setError("");
    let confirmed = false;
    try {
      await management.run(request);
      confirmed = true;
      if (request.action === "alias") {
        const identity = workspaceIds[request.serverId];
        if (identity) {
          const key = `server-alias:${identity}`;
          const previous =
            readPreferenceFields("user")[JSON.stringify([key])]?.value;
          capturePreferenceWrite(
            key,
            JSON.stringify(request.alias),
            JSON.stringify(previous ?? null),
          );
        }
      }
      if (
        !enabled &&
        request.action === "name" &&
        (!localAvailable || request.serverId !== "local")
      ) {
        const server = readServers().find((row) => row.id === request.serverId);
        if (server) await refreshServerName(server);
      }
      return true;
    } catch (failure) {
      setError(errorText(failure));
      return confirmed;
    } finally {
      try {
        setPending(management.pending()[0] || null);
      } catch (failure) {
        setError(errorText(failure));
      }
      busyRef.current = false;
      setBusy(false);
    }
  };
  useEffect(() => {
    const update = () => {
      try {
        setPending(management.pending()[0] || null);
      } catch (failure) {
        setError(errorText(failure));
      }
    };
    update();
    window.addEventListener("storage", update);
    return () => window.removeEventListener("storage", update);
  }, [enabled]);
  return {
    snapshot: effectiveSnapshot,
    checkedAt,
    error: [error, accessError].filter(Boolean).join("\n"),
    busy,
    actions: {
      find: () => {
        void run({ action: "discover", requestId: randomUUID() });
      },
      setAutoPair: (autoPair: boolean) => {
        void run({
          action: "settings",
          autoPair,
          requestId: randomUUID(),
        });
      },
      setAutoUpdate: (autoUpdate: boolean) => {
        void run({
          action: "settings",
          autoUpdate,
          requestId: randomUUID(),
        });
      },
    },
    setName: (serverId: string, label: string) =>
      run({ action: "name", serverId, label, requestId: randomUUID() }),
    setAlias: (serverId: string, alias: string) =>
      run({
        action: "alias",
        serverId,
        alias,
        requestId: randomUUID(),
      }),
    revoke: (id: string) => {
      void run({
        action: "revoke",
        clientId: id,
        requestId: randomUUID(),
      });
    },
    allowAgain: (id: string) => {
      void run({
        action: "unrevoke",
        clientId: id,
        requestId: randomUUID(),
      });
    },
    retry: pending
      ? () => {
          void run(pending);
        }
      : undefined,
    addAgain: (id: string) => {
      try {
        controller.current?.retry(id);
        if (snapshot)
          void controller.current
            ?.reconcile({
              ...snapshot,
              servers: snapshot.servers.filter((server) => server.id === id),
            })
            .then(
              () => setAccessError(""),
              (failure) => setAccessError(errorText(failure)),
            );
      } catch (failure) {
        setError(errorText(failure));
      }
    },
  };
}
