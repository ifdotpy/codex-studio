import StudioSettingsTabs, {
  isStudioSettingsTab,
} from "../components/StudioSettingsTabs";
import { ServerSettingsContext } from "./ServerSettingsContext";
import { useServerDiscovery } from "./useServerDiscovery";
import { modalSizes } from "../theme";
import {
  Button,
  TextInput,
  Modal,
  useMantineColorScheme,
  Tabs,
} from "@mantine/core";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import App from "../App";
import ProjectGroupsSidebar from "./ProjectGroupsSidebar";
import { projectChatCommand } from "./projectChatCommand";
import { frameURL } from "./frameOrigin";
import { canUnloadFrame } from "./idleFrames";
import { fetchServerSummary } from "./summary";
import { bindShellTransport } from "./shellTransport";
import { serverCredentialAdapter } from "./transport";
import {
  localServer,
  readServers,
  writeServers,
  SERVER_REGISTRY_EVENT,
  SERVER_REGISTRY_KEY,
  type StudioServer,
} from "./registry";
import type { ServerCommand, ServerNavigation } from "./navigation";
import type { ResourceConnectionState } from "../sync/resourceEvents";
import ServerManager from "./ServerManager";
import ServerAccountsPanel from "./ServerAccountsPanel";
import type { ServerAccount } from "./navigation";
import {
  studioPreferencesStorageKey,
  parseStudioPreferences,
  defaultStudioPreferences,
  parseSidebarShortcut,
  fontFamilies,
} from "../studioPreferences";
import type { GetResult } from "../api";
import "./servers.css";
const uiOnly =
  new URLSearchParams(location.search).get("studio-ui-only") === "1";
export default function MultiServerApp() {
  const [failure, setFailure] = useState("");
  const load = () => {
    try {
      return readServers();
    } catch (error) {
      setFailure(String((error as Error).message));
      return [];
    }
  };
  const [paired, setPaired] = useState<StudioServer[]>(load);
  const servers = useMemo(
    () => (uiOnly ? paired : [localServer(), ...paired]),
    [paired],
  );
  const [selected, setSelected] = useState(
    () =>
      localStorage.getItem("studio-selected-server") || servers[0]?.id || "",
  );
  const current = servers.some((server) => server.id === selected)
    ? selected
    : servers[0]?.id || "";
  const [manager, setManager] = useState(uiOnly && !paired.length);
  const managerRef = useRef(manager);
  managerRef.current = manager;
  const [managerTab, setManagerTab] = useState("servers");
  const restoreManagerAfterFrameDialog = useRef(false);
  const discovery = useServerDiscovery(
    !uiOnly && ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname),
    (server) => add(server, false),
  );
  const [navigation, setNavigation] = useState<
    Record<string, ServerNavigation>
  >({});
  const navigationRef = useRef(navigation);
  navigationRef.current = navigation;
  const [status, setStatus] = useState<Record<string, ResourceConnectionState>>(
    {},
  );
  const [accountsByServer, setAccountsByServer] = useState<
    Record<string, ServerAccount[]>
  >({});
  const [lastSeen, setLastSeen] = useState<Record<string, number>>({});
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  type SearchRow = GetResult<"/api/search">["results"][number];
  const [searchResults, setSearchResults] = useState<
    Record<string, { results: SearchRow[]; error?: string }>
  >({});
  const searchRequest = useRef("");
  const notificationState = useRef(new Map<string, Set<string>>());
  const [sidebarHidden, setSidebarHidden] = useState(false);
  const toggleSidebar = useCallback(() => {
    if (window.matchMedia("(max-width: 760px)").matches)
      setMobileOpen((old) => !old);
    else setSidebarHidden((old) => !old);
  }, []);
  const { setColorScheme } = useMantineColorScheme();
  const [preferences, setPreferences] = useState(() => {
    try {
      return parseStudioPreferences(
        localStorage.getItem(studioPreferencesStorageKey) || "null",
      );
    } catch {
      return defaultStudioPreferences;
    }
  });
  const preferencesRef = useRef(preferences);
  preferencesRef.current = preferences;
  const [query, setQuery] = useState("");
  const [mobileOpen, setMobileOpen] = useState(false);
  const frames = useRef(new Map<string, HTMLIFrameElement>());
  const pending = useRef(new Map<string, ServerCommand>());
  const [unloaded, setUnloaded] = useState(new Set<string>());
  const unloadedRef = useRef(unloaded);
  unloadedRef.current = unloaded;
  const currentRef = useRef(current);
  currentRef.current = current;
  const serversRef = useRef(servers);
  serversRef.current = servers;
  const lastSelected = useRef(current);
  const readyFrames = useRef(new Set<string>());
  const lifecycle = useRef(
    new Map<
      string,
      {
        idleSince: number;
        busy: boolean;
        activity: boolean;
        known: boolean;
        ready: boolean;
      }
    >(),
  );
  const life = (id: string) => {
    if (!lifecycle.current.has(id))
      lifecycle.current.set(id, {
        idleSince: Date.now(),
        busy: false,
        activity: false,
        known: false,
        ready: false,
      });
    return lifecycle.current.get(id)!;
  };
  useEffect(() => {
    life(lastSelected.current).idleSince = Date.now();
    life(current).idleSince = Date.now();
    lastSelected.current = current;
  }, [current]);
  const mount = (id: string) => {
    if (unloadedRef.current.has(id)) {
      readyFrames.current.delete(id);
      Object.assign(life(id), {
        idleSince: Date.now(),
        known: false,
        ready: false,
      });
    }
    setUnloaded((old) => {
      if (!old.has(id)) return old;
      const next = new Set(old);
      next.delete(id);
      return next;
    });
  };
  const targetOrigin = (id: string) =>
    new URL(frames.current.get(id)?.src || location.href).origin;
  useEffect(() => bindShellTransport(frames.current), []);
  const publishPreferences = (value: unknown) => {
    for (const [id, frame] of frames.current)
      frame.contentWindow?.postMessage(
        { kind: "studio-server-preferences", serverId: id, preferences: value },
        new URL(frame.src).origin,
      );
  };
  const select = useCallback((id: string) => {
    mount(id);
    life(id).idleSince = Date.now();
    setSelected(id);
    localStorage.setItem("studio-selected-server", id);
    setMobileOpen(false);
    requestAnimationFrame(() => {
      const frame = frames.current.get(id);
      frame?.focus();
      frame?.contentWindow?.postMessage(
        {
          kind: "studio-server-command",
          serverId: id,
          command: { action: "focus" },
        },
        targetOrigin(id),
      );
    });
  }, []);
  const send = (id: string, command: ServerCommand) => {
    select(id);
    const frame = frames.current.get(id);
    if (!frame || !readyFrames.current.has(id))
      pending.current.set(id, command);
    else
      frame.contentWindow?.postMessage(
        { kind: "studio-server-command", serverId: id, command },
        targetOrigin(id),
      );
  };
  const applyNavigation = (id: string, value: ServerNavigation) => {
    setNavigation((old) => ({ ...old, [id]: value }));
    if (value.ready) {
      const previous = notificationState.current.get(id);
      const alerts = value.alerts || [];
      const seen = previous || new Set<string>();
      const fresh = previous
        ? alerts.filter((alert) => !seen.has(alert.id))
        : [];
      for (const alert of alerts) seen.add(alert.id);
      notificationState.current.set(id, seen);
      for (const alert of fresh) {
        const server =
          readServers().find((row) => row.id === id) || localServer();
        const title = `${server.label}: ${alert.title}`.slice(0, 160);
        if (window.codexDesktop)
          void window.codexDesktop
            .notify({
              title,
              body: alert.body,
              target: { ...alert.target, serverId: id },
            })
            .catch(() => {});
        else if (
          "Notification" in window &&
          Notification.permission === "granted"
        ) {
          const notification = new Notification(title, {
            body: alert.body,
            tag: `${id}:${alert.id}`,
          });
          notification.onclick = () => {
            notification.close();
            send(id, { action: "notifications", ...alert.target });
          };
        }
      }
    }
    if (pending.current.has(id) && value.ready && frames.current.has(id)) {
      const command = pending.current.get(id)!;
      pending.current.delete(id);
      frames.current
        .get(id)
        ?.contentWindow?.postMessage(
          { kind: "studio-server-command", serverId: id, command },
          targetOrigin(id),
        );
    }
  };
  const applyNavigationRef = useRef(applyNavigation);
  applyNavigationRef.current = applyNavigation;
  useEffect(() => {
    const timer = setInterval(() => {
      const remove: string[] = [];
      for (const [id, state] of lifecycle.current) {
        if (id === currentRef.current || state.busy || state.activity)
          state.idleSince = Date.now();
        if (
          frames.current.has(id) &&
          canUnloadFrame(
            id === currentRef.current,
            state.ready && state.known,
            state.busy || state.activity,
            state.idleSince,
            Date.now(),
          )
        ) {
          remove.push(id);
          readyFrames.current.delete(id);
        }
      }
      if (remove.length) setUnloaded((old) => new Set([...old, ...remove]));
    }, 15000);
    return () => clearInterval(timer);
  }, []);
  useEffect(() => {
    const timers = new Set<ReturnType<typeof setTimeout>>();
    const controllers = new Set<AbortController>();
    let stopped = false;
    for (const server of serversRef.current.filter((row) =>
      unloaded.has(row.id),
    )) {
      let delay = 30000;
      const poll = async () => {
        const controller = new AbortController();
        controllers.add(controller);
        const deadline = setTimeout(() => controller.abort(), 15000);
        try {
          const value = await fetchServerSummary(server, controller.signal);
          if (
            stopped ||
            controller.signal.aborted ||
            !unloadedRef.current.has(server.id)
          )
            return;
          const opened = navigation[server.id]?.opened || null;
          applyNavigationRef.current(server.id, { ...value, opened });
          setAccountsByServer((old) => ({
            ...old,
            [server.id]: value.accounts,
          }));
          setStatus((old) => ({ ...old, [server.id]: "live" }));
          setLastSeen((old) => ({ ...old, [server.id]: Date.now() / 1000 }));
          delay = 30000;
        } catch {
          if (!stopped)
            setStatus((old) => ({ ...old, [server.id]: "offline" }));
          delay = Math.min(delay * 2, 120000);
        } finally {
          clearTimeout(deadline);
          controllers.delete(controller);
          if (!stopped) {
            const timer = setTimeout(() => {
              timers.delete(timer);
              void poll();
            }, delay);
            timers.add(timer);
          }
        }
      };
      void poll();
    }
    return () => {
      stopped = true;
      for (const timer of timers) clearTimeout(timer);
      for (const controller of controllers) controller.abort();
    };
  }, [unloaded, paired]);
  useEffect(() => {
    if (!manager || managerTab !== "servers") return;
    const controllers = new Set<AbortController>();
    let stopped = false;
    for (const server of servers) {
      const controller = new AbortController();
      controllers.add(controller);
      void fetchServerSummary(server, controller.signal)
        .then((value) => {
          if (stopped || controller.signal.aborted) return;
          setNavigation((old) => ({
            ...old,
            [server.id]: { ...old[server.id], ...value },
          }));
          setAccountsByServer((old) => ({
            ...old,
            [server.id]: value.accounts,
          }));
          setStatus((old) => ({ ...old, [server.id]: "live" }));
          setLastSeen((old) => ({ ...old, [server.id]: Date.now() / 1000 }));
        })
        .catch(() => {
          if (!stopped)
            setStatus((old) => ({ ...old, [server.id]: "offline" }));
        })
        .finally(() => controllers.delete(controller));
    }
    return () => {
      stopped = true;
      controllers.forEach((controller) => controller.abort());
    };
  }, [manager, managerTab, servers]);
  useEffect(() => {
    const stored = (event: StorageEvent) => {
      if (event.key === SERVER_REGISTRY_KEY || event.key === null)
        setPaired(load());
    };
    const changed = () => setPaired(load());
    window.addEventListener("storage", stored);
    window.addEventListener(SERVER_REGISTRY_EVENT, changed);
    return () => {
      window.removeEventListener("storage", stored);
      window.removeEventListener(SERVER_REGISTRY_EVENT, changed);
    };
  }, []);
  useEffect(() => {
    const apply = () => {
      try {
        const next = parseStudioPreferences(
          localStorage.getItem(studioPreferencesStorageKey) || "null",
        );
        setPreferences(next);
        publishPreferences(next);
      } catch {}
    };
    const key = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setSearchOpen(true);
      }
      const shortcut = parseSidebarShortcut(preferences.sidebarShortcut);
      if (
        shortcut &&
        event.key.toLowerCase() === shortcut.key &&
        event.metaKey === shortcut.meta &&
        event.ctrlKey === shortcut.ctrl &&
        event.altKey === shortcut.alt &&
        event.shiftKey === shortcut.shift &&
        !(event.target as HTMLElement)?.closest?.(
          "input,textarea,select,[contenteditable]",
        )
      ) {
        event.preventDefault();
        toggleSidebar();
      }
    };
    window.addEventListener("storage", apply);
    // Local App writes do not emit a storage event in this window.
    window.addEventListener("studio-preferences-change", apply);
    window.addEventListener("keydown", key);
    return () => {
      window.removeEventListener("storage", apply);
      window.removeEventListener("studio-preferences-change", apply);
      window.removeEventListener("keydown", key);
    };
  }, [preferences.sidebarShortcut, toggleSidebar]);
  useEffect(() => {
    setColorScheme(preferences.theme);
    document.documentElement.style.setProperty(
      "--studio-font-family",
      fontFamilies[preferences.fontFamily].css,
    );
    document.documentElement.style.setProperty(
      "--studio-sidebar-font-size",
      preferences.sidebarFontSize + "px",
    );
  }, [preferences, setColorScheme]);
  useEffect(() => {
    const receive = async (event: MessageEvent) => {
      const id =
        event.data?.serverId ||
        [...frames.current].find(
          ([, frame]) => frame.contentWindow === event.source,
        )?.[0];
      if (
        event.origin !== targetOrigin(id) ||
        typeof id !== "string" ||
        !frames.current.get(id)?.contentWindow ||
        event.source !== frames.current.get(id)?.contentWindow
      )
        return;
      if (event.data.kind === "studio-server-frame-credential-request") {
        if (
          event.data.serverId !== id ||
          typeof event.data.correlation !== "string"
        )
          return;
        try {
          const frame = frames.current.get(id);
          if (!frame?.isConnected || frame.contentWindow !== event.source)
            return;
          const server =
            id === "local"
              ? localServer()
              : serversRef.current.find((row) => row.id === id);
          if (!server) throw new Error("The server is no longer paired.");
          const adapter = serverCredentialAdapter();
          if (!adapter.frameSigningCredential)
            throw new Error("Frame signing is unavailable.");
          const credential = await adapter.frameSigningCredential(server);
          if (frames.current.get(id) !== frame || !frame.isConnected) return;
          frame.contentWindow?.postMessage(
            {
              kind: "studio-server-frame-credential-result",
              serverId: id,
              correlation: event.data.correlation,
              credential: { serverId: id, ...credential },
            },
            event.origin,
          );
        } catch (error) {
          const frame = frames.current.get(id);
          if (frame?.contentWindow !== event.source || !frame.isConnected)
            return;
          frame.contentWindow?.postMessage(
            {
              kind: "studio-server-frame-credential-result",
              serverId: id,
              correlation: event.data.correlation,
              error: (error as Error).message,
            },
            event.origin,
          );
        }
      } else if (
        event.data.kind === "studio-server-activity" &&
        typeof event.data.busy === "boolean"
      ) {
        const state = life(id);
        state.known = true;
        if (state.activity !== event.data.busy) state.idleSince = Date.now();
        state.activity = event.data.busy;
      } else if (event.data.kind === "studio-project-new-chat") {
        const target = serversRef.current.find(
          (server) => server.id === event.data.target,
        );
        const command =
          target && projectChatCommand(navigationRef.current[id], event.data);
        if (target && command) send(target.id, command);
      } else if (event.data.kind === "studio-server-account-dialog") {
        if (typeof event.data.opened !== "boolean") return;
        if (event.data.opened) {
          if (managerRef.current) {
            restoreManagerAfterFrameDialog.current = true;
            setManager(false);
          }
        } else if (restoreManagerAfterFrameDialog.current) {
          restoreManagerAfterFrameDialog.current = false;
          setManager(true);
        }
      } else if (event.data.kind === "studio-server-open-settings") {
        setManager(true);
      } else if (event.data.kind === "studio-server-preferences") {
        try {
          const value = parseStudioPreferences(
            JSON.stringify(event.data.preferences),
          );
          localStorage.setItem(
            studioPreferencesStorageKey,
            JSON.stringify(value),
          );
          setPreferences(value);
          publishPreferences(value);
        } catch {}
      } else if (event.data.kind === "studio-server-toggle-sidebar") {
        toggleSidebar();
      } else if (event.data.kind === "studio-server-search")
        setSearchOpen(true);
      else if (
        event.data.kind === "studio-server-search-results" &&
        event.data.requestId === searchRequest.current
      ) {
        setSearchResults((old) => ({
          ...old,
          [id]: { results: event.data.results || [], error: event.data.error },
        }));
      } else if (event.data.kind === "studio-server-navigation") {
        const value = event.data.navigation as ServerNavigation;
        publishPreferences(preferencesRef.current);
        if (
          !value ||
          !Array.isArray(value.projects) ||
          !Array.isArray(value.chats)
        )
          return;
        const state = life(id);
        if (state.busy !== !!value.busy) state.idleSince = Date.now();
        state.busy = !!value.busy;
        state.ready = value.ready;
        if (value.ready) readyFrames.current.add(id);
        applyNavigationRef.current(id, value);
      } else if (event.data.kind === "studio-server-accounts") {
        if (
          !Array.isArray(event.data.accounts) ||
          event.data.accounts.length > 500
        )
          return;
        const accounts: ServerAccount[] = [];
        for (const row of event.data.accounts) {
          if (
            !row ||
            (row.provider !== "codex" && row.provider !== "claude") ||
            (row.email !== null && typeof row.email !== "string") ||
            (row.plan !== null && typeof row.plan !== "string") ||
            typeof row.status !== "string" ||
            typeof row.label !== "string" ||
            typeof row.isDefault !== "boolean"
          )
            return;
          accounts.push({
            provider: row.provider,
            email: row.email,
            plan: row.plan,
            status: row.status,
            label: row.label,
            isDefault: row.isDefault,
          });
        }
        setAccountsByServer((old) => ({ ...old, [id]: accounts }));
      } else if (
        event.data.kind === "studio-server-status" &&
        [
          "connecting",
          "live",
          "offline",
          "degraded",
          "schema-mismatch",
        ].includes(event.data.status)
      ) {
        setStatus((old) => ({ ...old, [id]: event.data.status }));
        if (event.data.status === "live")
          setLastSeen((old) => ({ ...old, [id]: Date.now() / 1000 }));
      }
    };
    window.addEventListener("message", receive);
    return () => window.removeEventListener("message", receive);
  }, [toggleSidebar]);
  useEffect(
    () =>
      window.codexDesktop?.onNavigate((target) => {
        if (!target.serverId) return;
        send(target.serverId, { action: "notifications", ...target });
      }),
    [select],
  );
  const add = (server: StudioServer, focus = true) => {
    if (server.id === "local")
      throw new Error("The remote server returned an invalid identity.");
    const old = readServers();
    const same = old.find((row) => row.id === server.id);
    if (same && same.origin !== server.origin)
      throw new Error("This server identity belongs to another address.");
    writeServers([...old.filter((row) => row.id !== server.id), server]);
    if (focus) select(server.id);
  };
  const remove = (server: StudioServer) => {
    writeServers(readServers().filter((row) => row.id !== server.id));
    pending.current.delete(server.id);
    setAccountsByServer((old) => {
      const next = { ...old };
      delete next[server.id];
      return next;
    });
  };
  const management = (
    <ServerManager
      servers={servers}
      add={add}
      remove={remove}
      discovery={discovery}
      localEnabled={
        !uiOnly &&
        ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname)
      }
      statuses={status}
      lastSeen={lastSeen}
      navigation={navigation}
      accountsByServer={accountsByServer}
    />
  );
  const settings = (
    <Modal
      opened={manager}
      onClose={() => setManager(false)}
      size={modalSizes.settings}
      title="Studio settings"
      classNames={{ body: "studio-settings-body" }}
    >
      <div className="studio-settings-panel" data-testid="studio-settings">
        <Tabs
          value={managerTab}
          className="studio-settings-tabs"
          onChange={(tab) => {
            if (!isStudioSettingsTab(tab)) return;
            setManagerTab(tab);
            if (tab === "accounts" || tab === "servers") return;
            setManager(false);
            send(current, { action: "settings", tab });
          }}
        >
          <StudioSettingsTabs />
          <Tabs.Panel value="accounts" pt="md">
            <ServerAccountsPanel
              servers={servers}
              accountsByServer={accountsByServer}
              statuses={status}
              startAccount={(id, command) => {
                restoreManagerAfterFrameDialog.current = manager;
                setManager(false);
                send(id, command);
              }}
              accountAction={(id, command) => {
                restoreManagerAfterFrameDialog.current = manager;
                setManager(false);
                send(id, command);
              }}
            />
          </Tabs.Panel>
          <Tabs.Panel value="servers" pt="md">
            {management}
          </Tabs.Panel>
        </Tabs>
      </div>
    </Modal>
  );
  if (!uiOnly && !paired.length)
    return (
      <ServerSettingsContext.Provider
        value={{
          panel: management,
          activate: () => setManager(true),
          close: () => setManager(false),
        }}
      >
        <App />
      </ServerSettingsContext.Provider>
    );
  const unread = servers.reduce(
    (count, server) =>
      count +
      (navigation[server.id]?.chats.filter((chat) => chat.unread).length || 0),
    0,
  );
  return (
    <div
      className={`multi-server-shell ${sidebarHidden ? "server-sidebar-hidden" : ""}`}
    >
      <Button
        className="server-mobile-toggle"
        onClick={() => setMobileOpen(!mobileOpen)}
        aria-expanded={mobileOpen}
      >
        Servers and chats
      </Button>
      <aside
        className={`server-sidebar ${mobileOpen ? "server-sidebar-open" : ""}`}
        aria-label="Servers and projects"
      >
        <header>
          <strong>
            Studio servers{" "}
            {unread > 0 && (
              <span aria-label={`${unread} unread chats`}>({unread})</span>
            )}
          </strong>
          <Button
            size="xs"
            variant="subtle"
            onClick={() => setManager(true)}
            aria-label="Studio settings"
          >
            Settings
          </Button>
        </header>
        <Button variant="subtle" onClick={() => setSearchOpen(true)}>
          Search all messages
        </Button>
        <TextInput
          label="Find projects and chats"
          value={query}
          onChange={(event) => setQuery(event.currentTarget.value)}
        />
        {failure && <p role="alert">{failure}</p>}
        {!servers.length && (
          <p>Pair a server to open its projects and chats.</p>
        )}
        <Button
          variant="subtle"
          onClick={() =>
            send(
              servers.some((row) => row.id === "local") ? "local" : current,
              { action: "add-project" },
            )
          }
        >
          Add project
        </Button>
        <ProjectGroupsSidebar
          navigation={navigation}
          servers={servers}
          statuses={status}
          serverAliases={discovery.snapshot?.aliases}
          reachability={Object.fromEntries(
            (discovery.snapshot?.servers || []).map((server) => [
              server.id,
              server.reachability,
            ]),
          )}
          current={current}
          query={query}
          send={send}
        />
      </aside>
      <main className="server-views">
        {!servers.length && (
          <div className="startup">
            <p>No server is paired.</p>
            <Button onClick={() => setManager(true)}>Open Settings</Button>
          </div>
        )}
        {servers
          .filter((server) => !unloaded.has(server.id))
          .map((server) => (
            <iframe
              key={`${server.id}:${server.credentialId || "local"}`}
              title={`Studio on ${server.label}`}
              ref={(frame) => {
                if (frame) frames.current.set(server.id, frame);
                else frames.current.delete(server.id);
              }}
              src={frameURL(server)}
              hidden={current !== server.id}
              allow="microphone; clipboard-read; clipboard-write"
            />
          ))}
      </main>
      {settings}
      <Modal
        opened={searchOpen}
        onClose={() => setSearchOpen(false)}
        title="Search all servers"
        aria-label="Search all servers"
        size="lg"
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (!searchQuery.trim()) return;
            const requestId = crypto.randomUUID();
            searchRequest.current = requestId;
            setSearchResults({});
            for (const server of servers) {
              mount(server.id);
              life(server.id).idleSince = Date.now();
              const command: ServerCommand = {
                action: "search",
                requestId,
                query: searchQuery.trim(),
              };
              if (
                !frames.current.has(server.id) ||
                !readyFrames.current.has(server.id)
              ) {
                pending.current.set(server.id, command);
                continue;
              }
              frames.current.get(server.id)?.contentWindow?.postMessage(
                {
                  kind: "studio-server-command",
                  serverId: server.id,
                  command: {
                    action: "search",
                    requestId,
                    query: searchQuery.trim(),
                  },
                },
                targetOrigin(server.id),
              );
            }
          }}
        >
          <TextInput
            autoFocus
            label="Search all messages"
            value={searchQuery}
            onChange={(event) => setSearchQuery(event.currentTarget.value)}
          />
          <Button type="submit" disabled={!searchQuery.trim()}>
            Search
          </Button>
        </form>
        {searchRequest.current &&
          servers.map((server) => (
            <section key={server.id}>
              <h3>{server.label}</h3>
              {!searchResults[server.id] && (
                <p role="status">Searching this server...</p>
              )}
              {searchResults[server.id]?.error && (
                <p role="alert">{searchResults[server.id].error}</p>
              )}
              {searchResults[server.id]?.results.map((result, index) => (
                <button
                  className="server-search-result"
                  key={`${result.id}:${index}`}
                  onClick={() => {
                    send(server.id, {
                      action: "open",
                      id: result.room || result.agent,
                      messageId: result.id,
                    });
                    setSearchOpen(false);
                  }}
                >
                  <small>{result.kind}</small>
                  <p>{result.text}</p>
                </button>
              ))}
              {searchResults[server.id]?.results.length === 0 &&
                !searchResults[server.id]?.error && <p>No results.</p>}
            </section>
          ))}
      </Modal>
    </div>
  );
}
