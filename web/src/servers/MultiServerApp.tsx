import { Button, TextInput, Modal, useMantineColorScheme } from "@mantine/core";
import { useCallback, useEffect, useRef, useState } from "react";
import App from "../App";
import { frameURL } from "./frameOrigin";
import { bindShellTransport } from "./shellTransport";
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
  const servers = uiOnly ? paired : [localServer(), ...paired];
  const [selected, setSelected] = useState(
    () =>
      localStorage.getItem("studio-selected-server") || servers[0]?.id || "",
  );
  const current = servers.some((server) => server.id === selected)
    ? selected
    : servers[0]?.id || "";
  const [manager, setManager] = useState(uiOnly && !paired.length);
  const [navigation, setNavigation] = useState<
    Record<string, ServerNavigation>
  >({});
  const [status, setStatus] = useState<Record<string, ResourceConnectionState>>(
    {},
  );
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
    if (!frame || !navigation[id]) pending.current.set(id, command);
    else
      frame.contentWindow?.postMessage(
        { kind: "studio-server-command", serverId: id, command },
        targetOrigin(id),
      );
  };
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
    window.addEventListener("keydown", key);
    return () => {
      window.removeEventListener("storage", apply);
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
    const receive = (event: MessageEvent) => {
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
      if (event.data.kind === "studio-server-preferences") {
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
                setSelected(id);
                setMobileOpen(false);
                notification.close();
                frames.current.get(id)?.contentWindow?.postMessage(
                  {
                    kind: "studio-server-command",
                    serverId: id,
                    command: { action: "notifications", ...alert.target },
                  },
                  targetOrigin(id),
                );
              };
            }
          }
        }
        if (pending.current.has(id) && value.ready) {
          const command = pending.current.get(id)!;
          pending.current.delete(id);
          frames.current
            .get(id)
            ?.contentWindow?.postMessage(
              { kind: "studio-server-command", serverId: id, command },
              targetOrigin(id),
            );
        }
      } else if (
        event.data.kind === "studio-server-status" &&
        [
          "connecting",
          "live",
          "offline",
          "degraded",
          "schema-mismatch",
        ].includes(event.data.status)
      )
        setStatus((old) => ({ ...old, [id]: event.data.status }));
    };
    window.addEventListener("message", receive);
    return () => window.removeEventListener("message", receive);
  }, [toggleSidebar]);
  useEffect(
    () =>
      window.codexDesktop?.onNavigate((target) => {
        if (!target.serverId) return;
        select(target.serverId);
        frames.current.get(target.serverId)?.contentWindow?.postMessage(
          {
            kind: "studio-server-command",
            serverId: target.serverId,
            command: { action: "notifications", ...target },
          },
          targetOrigin(target.serverId),
        );
      }),
    [select],
  );
  const add = (server: StudioServer) => {
    if (server.id === "local")
      throw new Error("The remote server returned an invalid identity.");
    const old = readServers();
    const same = old.find((row) => row.id === server.id);
    if (same && same.origin !== server.origin)
      throw new Error("This server identity belongs to another address.");
    writeServers([...old.filter((row) => row.id !== server.id), server]);
    select(server.id);
  };
  const remove = (server: StudioServer) => {
    writeServers(readServers().filter((row) => row.id !== server.id));
    pending.current.delete(server.id);
  };
  const management = (
    <ServerManager
      opened={manager}
      close={() => setManager(false)}
      servers={servers}
      add={add}
      remove={remove}
    />
  );
  if (!uiOnly && !paired.length)
    return (
      <>
        <App />
        <Button
          className="server-manager-launch"
          size="xs"
          onClick={() => setManager(true)}
        >
          Servers
        </Button>
        {management}
      </>
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
          <Button size="xs" variant="subtle" onClick={() => setManager(true)}>
            Manage
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
        {servers.map((server) => {
          const view = navigation[server.id];
          const needle = query.toLocaleLowerCase();
          return (
            <section key={server.id} data-server={server.id}>
              <header>
                <button
                  className="server-title"
                  onClick={() => select(server.id)}
                  aria-current={current === server.id ? "true" : undefined}
                >
                  {server.label}
                </button>
                <small role="status">
                  {status[server.id] === "live"
                    ? "Online"
                    : status[server.id] === "schema-mismatch"
                      ? "Update required"
                      : status[server.id] === "offline" ||
                          status[server.id] === "degraded"
                        ? "Offline"
                        : "Connecting"}
                </small>
              </header>
              <div className="server-tools">
                <Button
                  size="xs"
                  variant="subtle"
                  onClick={() => send(server.id, { action: "projects" })}
                >
                  Projects
                </Button>
                <Button
                  size="xs"
                  variant="subtle"
                  onClick={() => send(server.id, { action: "settings" })}
                >
                  Settings
                </Button>
              </div>
              {view?.projects.map((project) => {
                const chats = view.chats.filter(
                  (chat) =>
                    chat.path === project.path &&
                    !chat.archived &&
                    (!needle ||
                      `${server.label} ${project.name} ${project.path} ${chat.name}`
                        .toLocaleLowerCase()
                        .includes(needle)),
                );
                if (
                  needle &&
                  !chats.length &&
                  !`${server.label} ${project.name} ${project.path}`
                    .toLocaleLowerCase()
                    .includes(needle)
                )
                  return null;
                return (
                  <details key={project.path} open>
                    <summary title={`${project.path} (${server.label})`}>
                      {project.name}
                      <small>{server.label}</small>
                    </summary>
                    <Button
                      size="xs"
                      variant="subtle"
                      onClick={() =>
                        send(server.id, {
                          action: "new-chat",
                          path: project.path || undefined,
                        })
                      }
                    >
                      New chat
                    </Button>
                    {chats.map((chat) => (
                      <button
                        className="server-chat"
                        key={chat.id}
                        data-chat={chat.id}
                        aria-current={
                          current === server.id && view.opened === chat.id
                            ? "page"
                            : undefined
                        }
                        onClick={() =>
                          send(server.id, { action: "open", id: chat.id })
                        }
                      >
                        {chat.name}
                        {chat.unread && (
                          <span aria-label="Unread result"> ●</span>
                        )}
                      </button>
                    ))}
                  </details>
                );
              })}
              {view?.error && <p className="notice">{view.error}</p>}
            </section>
          );
        })}
      </aside>
      <main className="server-views">
        {!servers.length && (
          <div className="startup">
            <p>No server is paired.</p>
            <Button onClick={() => setManager(true)}>Pair server</Button>
          </div>
        )}
        {servers.map((server) => (
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
      {management}
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
            for (const server of servers)
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
