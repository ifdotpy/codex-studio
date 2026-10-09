import {
  capturePreferenceWrite,
  readPreferenceFields,
} from "../sync/uiPreferenceStore";
import { ServerManagementRequests } from "./managementRequests";
import { useEffect, useRef, useState } from "react";
import { errorText, refreshSession, serverAccess } from "../api";
import { discoverySnapshot, type DiscoverySnapshot } from "./discoveryModel";
import type { ServerAccessRequest, ServerAccessState } from "./accessContract";
import { AutomaticUiAccess } from "./automaticUiAccess";
import {
  automaticAccessStore,
  excludedServers,
  setServerExcluded,
} from "./automaticAccessStore";
import { readServers, writeServers, type StudioServer } from "./registry";
import { LOCAL_ALIAS_KEY } from "./serverAliases";
import { serverCredentialAdapter } from "./transport";
export const DISCOVERY_REFRESH_EVENT = "studio-server-discovery-refresh";
export function useServerDiscovery(
  enabled: boolean,
  add: (server: StudioServer) => void,
) {
  const [snapshot, setSnapshot] = useState<DiscoverySnapshot | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [checkedAt, setCheckedAt] = useState<number | null>(null);
  const [excluded, setExcluded] = useState<Set<string>>(new Set());
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
  useEffect(() => {
    let mounted = true;
    void attempts.current!.initialize().catch((failure) => {
      if (mounted) setError(errorText(failure));
    });
    return () => {
      mounted = false;
    };
  }, []);
  const controller = useRef<AutomaticUiAccess | null>(null);
  if (!controller.current)
    controller.current = new AutomaticUiAccess(attempts.current, {
      current: async () => {
        const value = await serverAccess("GET");
        if (!("identity" in value))
          throw new Error("The server discovery response is invalid.");
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
      excluded: (id) => excludedServers().has(id),
      add: (server) => {
        if (active.current) addRef.current(server);
      },
      pair: (...args) => serverCredentialAdapter().pair(...args),
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
      const session = await refreshSession();
      if (body.action === "revoke") controller.current?.cancel(body.clientId);
      await serverAccess(
        "POST",
        body,
        body.action === "discover" ? 105000 : 15000,
        session.token,
      );
    },
  );
  const loading = useRef<Promise<void> | null>(null);
  const apply = async (state: ServerAccessState) => {
    const next = discoverySnapshot(state);
    if (!active.current) return;
    if (next?.aliases) {
      const aliases = { ...next.aliases };
      const fields = readPreferenceFields("user");
      for (const id of ["local", ...readServers().map((server) => server.id)]) {
        const identity = localStorage.getItem(
          `studio-server-workspace-id:${id}`,
        );
        const value =
          identity &&
          fields[JSON.stringify([`server-alias:${identity}`])]?.value;
        if (typeof value === "string") aliases[id] = value;
      }
      next.aliases = aliases;
      if (aliases.local) localStorage.setItem(LOCAL_ALIAS_KEY, aliases.local);
      const paired = readServers().map((server) =>
        aliases[server.id] ? { ...server, alias: aliases[server.id] } : server,
      );
      writeServers(paired);
    }
    setSnapshot(next);
    setCheckedAt(Date.now() / 1000);
    setExcluded(excludedServers());
    if (next)
      void controller.current!.reconcile(next).catch((failure) => {
        if (active.current) setError(errorText(failure));
      });
  };
  const load = () => {
    if (loading.current) return loading.current;
    loading.current = (async () => {
      await attempts.current!.initialize();
      const value = await serverAccess("GET");
      if (!("identity" in value))
        throw new Error("The server discovery response is invalid.");
      await apply(value);
    })().finally(() => {
      loading.current = null;
    });
    return loading.current;
  };
  const loadRef = useRef(load);
  loadRef.current = load;
  useEffect(() => {
    if (!enabled) return;
    active.current = true;
    let timer: ReturnType<typeof setTimeout>;
    let stopped = false,
      delay = 30000;
    const poll = async () => {
      try {
        await loadRef.current();
        if (!stopped) setError("");
        delay = 30000;
      } catch (failure) {
        if (!stopped) setError(errorText(failure));
        delay = Math.min(delay * 2, 120000);
      }
      if (!stopped) timer = setTimeout(poll, delay);
    };
    const refresh = () => {
      void loadRef.current().catch((failure) => setError(errorText(failure)));
    };
    window.addEventListener(DISCOVERY_REFRESH_EVENT, refresh);
    void poll();
    return () => {
      stopped = true;
      active.current = false;
      controller.current?.stop();
      clearTimeout(timer);
      window.removeEventListener(DISCOVERY_REFRESH_EVENT, refresh);
    };
  }, [enabled]);
  const run = async (request: ServerAccessRequest) => {
    if (!enabled || busyRef.current) return false;
    busyRef.current = true;
    setBusy(true);
    setError("");
    try {
      await management.run(request);
      if (request.action === "alias") {
        const identity = localStorage.getItem(
          `studio-server-workspace-id:${request.serverId}`,
        );
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
      // A retry receipt contains the original snapshot. Read the current state.
      if (loading.current) await loading.current;
      await load();
      return true;
    } catch (failure) {
      setError(errorText(failure));
      return false;
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
    if (!enabled) return;
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
    snapshot,
    checkedAt,
    error,
    busy,
    excluded,
    actions: {
      find: () => {
        void run({ action: "discover", requestId: crypto.randomUUID() });
      },
      setAutoPair: (autoPair: boolean) => {
        void run({
          action: "settings",
          autoPair,
          requestId: crypto.randomUUID(),
        });
      },
    },
    setAlias: (serverId: string, alias: string) =>
      run({
        action: "alias",
        serverId,
        alias,
        requestId: crypto.randomUUID(),
      }),
    revoke: (id: string) => {
      void run({
        action: "revoke",
        clientId: id,
        requestId: crypto.randomUUID(),
      });
    },
    allowAgain: (id: string) => {
      void run({
        action: "unrevoke",
        clientId: id,
        requestId: crypto.randomUUID(),
      });
    },
    retry: pending
      ? () => {
          void run(pending);
        }
      : undefined,
    addAgain: (id: string) => {
      try {
        setServerExcluded(id, false);
        controller.current?.retry(id);
        setExcluded(excludedServers());
        void load().catch((failure) => setError(errorText(failure)));
      } catch (failure) {
        setError(errorText(failure));
      }
    },
  };
}
