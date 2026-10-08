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
import { readServers, type StudioServer } from "./registry";
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
  const controller = useRef<AutomaticUiAccess | null>(null);
  if (!controller.current)
    controller.current = new AutomaticUiAccess(automaticAccessStore(), {
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
        await refreshSession();
        const value = await serverAccess("POST", {
          action: "ui_invite",
          serverId: server.id,
          requestId,
        });
        if (!("invitation" in value))
          throw new Error("The server invitation response is invalid.");
        return value.invitation;
      },
    });
  const pending = useRef<ServerAccessRequest | null>(null);
  const loading = useRef<Promise<void> | null>(null);
  const apply = async (state: ServerAccessState) => {
    const next = discoverySnapshot(state);
    if (!active.current) return;
    setSnapshot(next);
    setCheckedAt(Date.now() / 1000);
    setExcluded(excludedServers());
    if (next) await controller.current!.reconcile(next);
  };
  const load = () => {
    if (loading.current) return loading.current;
    loading.current = (async () => {
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
    if (!enabled || busy) return;
    if (
      pending.current &&
      JSON.stringify({ ...pending.current, requestId: "" }) !==
        JSON.stringify({ ...request, requestId: "" })
    ) {
      setError(
        "Retry the saved discovery request before you start another request.",
      );
      return;
    }
    const key = "studio-server-discovery-pending-v1";
    const body = pending.current || request;
    try {
      localStorage.setItem(key, JSON.stringify(body));
      pending.current = body;
    } catch {
      setError("The request could not be saved. No server setting changed.");
      return;
    }
    setBusy(true);
    setError("");
    try {
      await refreshSession();
      await serverAccess(
        "POST",
        body,
        body.action === "discover" ? 105000 : 15000,
      );
      pending.current = null;
      localStorage.removeItem(key);
      // A retry receipt contains the original snapshot. Read the current state.
      if (loading.current) await loading.current;
      await load();
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      setBusy(false);
    }
  };
  useEffect(() => {
    if (!enabled) return;
    try {
      pending.current = JSON.parse(
        localStorage.getItem("studio-server-discovery-pending-v1") || "null",
      );
    } catch (failure) {
      setError(errorText(failure));
    }
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
    retry: pending.current
      ? () => {
          void run(pending.current!);
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
