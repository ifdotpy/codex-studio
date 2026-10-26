import { ScreenContext } from "./ScreenContext";
import {
  createScreenChannel,
  type ScreenChannel,
  type ScreenMessage,
} from "./screenChannel";
import { ServerServicesProvider } from "./ServerServicesContext";
import { createServerServices, type ServerServices } from "./serverServices";
import {
  hasDesktopNotifications,
  notifyDesktop,
  onDesktopNavigate,
} from "../native/notifications";
import { orderPreferenceServers } from "./preferenceServerOrder";
import { writePreferenceEdit } from "../sync/uiPreferenceStore";
import { useUiPreferenceSync } from "./useUiPreferenceSync";
import {
  StudioSettingsLayout,
  isStudioSettingsTab,
  type StudioSettingsTab,
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
  ActionIcon,
} from "@mantine/core";
import { ChevronUp, Plus, Terminal as TerminalIcon } from "lucide-react";
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import App from "../App";
import { DEFAULT_LOCAL_SERVER_ALIAS } from "./serverAliases";
import CombinedServerSidebar from "./CombinedServerSidebar";
import {
  createMergedSidebarSelector,
  selectCombinedSidebar,
  createSidebarSourceStore,
} from "./mergedSidebar";
import { serverSidebarBackend } from "./serverSidebarBackend";
import ProjectServerCards from "../components/ProjectServerCards";
import { projectServerChoices, type LocationProject } from "./projectLocations";
import { projectChatCommand } from "./projectChatCommand";
import { fetchServerSummary, summaryFailure } from "./summary";
import {
  localServer,
  readServers,
  shareServers,
  writeServers,
  SERVER_REGISTRY_EVENT,
  SERVER_REGISTRY_KEY,
  type StudioServer,
} from "./registry";
import type { ServerCommand, ServerNavigation } from "./navigation";
import type { ResourceConnectionState } from "../sync/subscriptions/events";
import ServerManager from "./ServerManager";
import ServerAccountsPanel from "./ServerAccountsPanel";
import { accountServers } from "./accountServers";
import type { ServerAccount } from "./navigation";
import {
  studioPreferencesStorageKey,
  parseStudioPreferences,
  defaultStudioPreferences,
  parseSidebarShortcut,
  fontFamilies,
} from "../studioPreferences";
import type { GetResult } from "../api";
import { alertsSince } from "../desktop/desktopAlerts";
import "./servers.css";
const uiOnly =
  new URLSearchParams(location.search).get("studio-ui-only") === "1";
export default function MultiServerApp() {
  // Pairing changes the shell, but must keep the local database and transport owner.
  const [localServices] = useState(() =>
    uiOnly ? undefined : createServerServices(localServer()),
  );
  const shell = <MultiServerShell localServices={localServices} />;
  return localServices ? (
    <ServerServicesProvider services={localServices}>
      {shell}
    </ServerServicesProvider>
  ) : (
    shell
  );
}

function MultiServerShell({
  localServices,
}: {
  localServices?: ServerServices | undefined;
}) {
  const [failure, setFailure] = useState("");
  const [newChatDialog, setNewChatDialog] = useState<{
    project: LocationProject;
    server: string;
    folder?: string | undefined;
    binding?: { projectId: string; projectServerId: string } | undefined;
  } | null>(null);
  const load = () => {
    try {
      return readServers();
    } catch (error) {
      setFailure(String((error as Error).message));
      return [];
    }
  };
  const [paired, setPaired] = useState<StudioServer[]>(load);
  // Discovery and preference updates reread the saved list often. Unchanged servers keep
  // their objects, so an open dialog or a server connection is not reset by a reread.
  const [registryRevision, setRegistryRevision] = useState(0);
  const sharedServers = useRef<StudioServer[]>([]);
  const servers = useMemo(
    () =>
      (sharedServers.current = shareServers(
        sharedServers.current,
        orderPreferenceServers(uiOnly ? paired : [localServer(), ...paired]),
      )),
    [paired, registryRevision],
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
  const [managerTab, setManagerTab] = useState<StudioSettingsTab>(
    uiOnly ? "servers" : "accounts",
  );
  const openManager = () => {
    setManagerTab("servers");
    setManager(true);
  };
  const restoreManagerAfterAccountDialog = useRef(false);
  const restoreAccountActionFocus = useRef<string | null>(null);
  const discovery = useServerDiscovery(
    !uiOnly && ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname),
    (server) => add(server, false),
    !uiOnly,
  );
  const accountsServers = useMemo(
    () => accountServers(servers, discovery.snapshot?.servers || []),
    [servers, discovery.snapshot],
  );
  const [navigation, setNavigation] = useState<
    Record<string, ServerNavigation>
  >({});
  const sidebarSources = useRef(createSidebarSourceStore());
  const navigationRef = useRef(navigation);
  navigationRef.current = navigation;
  const [status, setStatus] = useState<Record<string, ResourceConnectionState>>(
    {},
  );
  const statusRef = useRef(status);
  statusRef.current = status;
  const [accountsByServer, setAccountsByServer] = useState<
    Record<string, ServerAccount[]>
  >({});
  const [lastSeen, setLastSeen] = useState<Record<string, number>>({});
  useUiPreferenceSync(servers);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  type SearchRow = GetResult<"/api/search">["results"][number];
  const [searchResults, setSearchResults] = useState<
    Record<string, { results: SearchRow[]; error?: string }>
  >({});
  const searchRequest = useRef("");
  const notificationState = useRef(
    new Map<string, { seen: Set<string>; started: number }>(),
  );
  const [sidebarHidden, setSidebarHidden] = useState(false);
  // Each server view reports its terminal panel; the shell draws the closed bar under
  // its sidebar and makes room for the open panel (servers.css "Terminal under the sidebar").
  const [terminals, setTerminals] = useState<
    Record<
      string,
      {
        opened: boolean;
        height: number;
        count: number;
        active: number;
        canCreate: boolean;
      }
    >
  >({});
  const [shellSidebarWidth, setShellSidebarWidth] = useState(0);
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
  const [mobileOpen, setMobileOpen] = useState(
    () => window.matchMedia("(max-width: 760px)").matches,
  );
  const screens = useRef(new Map<string, ScreenChannel>());
  const screenEvents = useRef<
    (owner: string, data: ScreenMessage, channel: ScreenChannel) => void
  >(() => {});
  const pending = useRef(new Map<string, ServerCommand>());
  const currentRef = useRef(current);
  currentRef.current = current;
  const serversRef = useRef(servers);
  serversRef.current = servers;
  const readyScreens = useRef(new Set<string>());
  const readyWaiters = useRef(new Map<string, Set<() => void>>());
  const publishPreferences = (value: unknown) => {
    window.dispatchEvent(
      new CustomEvent("studio-shell-preferences", { detail: value }),
    );
  };
  const select = useCallback((id: string) => {
    setSelected(id);
    localStorage.setItem("studio-selected-server", id);
    setMobileOpen(false);
    requestAnimationFrame(() => {
      if (currentRef.current !== id) return;
      const screen = screens.current.get(id);
      if (screen?.ready)
        void screen.execute({ action: "focus" }).catch(() => {});
    });
  }, []);
  const send = (id: string, command: ServerCommand) => {
    select(id);
    const screen = screens.current.get(id);
    if (!screen?.ready || !readyScreens.current.has(id))
      pending.current.set(id, command);
    else
      void screen.execute(command).catch((error) => setFailure(String(error)));
  };
  const applyNavigation = (id: string, value: ServerNavigation) => {
    sidebarSources.current.update(id, value.sidebar);
    if (value.ready)
      readyWaiters.current.get(id)?.forEach((resolve) => resolve());
    setNavigation((old) => ({ ...old, [id]: value }));
    if (value.ready && value.alertsReady !== false) {
      const previous = notificationState.current.get(id);
      const alerts = value.alerts || [];
      const known = previous || {
        seen: new Set<string>(),
        started: Date.now(),
      };
      const fresh = previous
        ? alerts
            .filter((alert) => !known.seen.has(alert.id))
            .filter(alertsSince(known.started))
        : [];
      for (const alert of alerts) known.seen.add(alert.id);
      notificationState.current.set(id, known);
      for (const alert of fresh) {
        const server =
          readServers().find((row) => row.id === id) || localServer();
        const title = `${server.label}: ${alert.title}`.slice(0, 160);
        if (hasDesktopNotifications())
          void notifyDesktop({
            title,
            body: alert.body,
            ...(alert.sound ? { sound: true } : {}),
            target: { ...alert.target, serverId: id },
          }).catch(() => {});
        else if (
          "Notification" in window &&
          Notification.permission === "granted"
        ) {
          const notification = new Notification(title, {
            body: alert.body,
            tag: `${id}:${alert.id}`,
            silent: !alert.sound,
          });
          notification.onclick = () => {
            notification.close();
            send(id, { action: "notifications", ...alert.target });
          };
        }
      }
    }
    const screen = screens.current.get(id);
    if (pending.current.has(id) && value.ready && screen?.ready) {
      const command = pending.current.get(id)!;
      pending.current.delete(id);
      void screen.execute(command).catch((error) => setFailure(String(error)));
    }
  };
  const applyNavigationRef = useRef(applyNavigation);
  applyNavigationRef.current = applyNavigation;
  const executeCommand = useCallback(
    async (owner: string, command: ServerCommand) => {
      const screen = screens.current.get(owner);
      if (!screen) throw new Error("The server is no longer paired.");
      if (!readyScreens.current.has(owner) || !screen.ready) {
        await new Promise<void>((resolve, reject) => {
          const waiters =
            readyWaiters.current.get(owner) || new Set<() => void>();
          const done = () => {
            clearTimeout(timer);
            waiters.delete(done);
            resolve();
          };
          const timer = setTimeout(() => {
            waiters.delete(done);
            reject(new Error("The server screen is unavailable."));
          }, 15000);
          waiters.add(done);
          readyWaiters.current.set(owner, waiters);
        });
      }
      if (screens.current.get(owner) !== screen)
        throw new Error(
          "The server connection changed. The action was not sent.",
        );
      await screen.execute(command);
    },
    [],
  );
  const sidebarOnline = useCallback(
    (owner: string) =>
      serversRef.current.some((server) => server.id === owner) &&
      statusRef.current[owner] === "live",
    [],
  );
  const serviceInstances = useRef(new Map<string, ServerServices>());
  const services = useMemo(
    () =>
      new Map(
        servers.map((server) => {
          const previous = serviceInstances.current.get(server.id);
          const instance =
            previous &&
            previous.owner.origin === server.origin &&
            previous.owner.credentialId === server.credentialId
              ? previous
              : server.id === "local" && localServices
                ? localServices
                : createServerServices(server);
          return [server.id, instance] as const;
        }),
      ),
    [servers, localServices],
  );
  const screenOwners = useRef(
    new Map<string, { service: ServerServices; channel: ScreenChannel }>(),
  );
  const channels = useMemo(
    () =>
      new Map(
        [...services].map(([id, service]) => {
          const previous = screenOwners.current.get(id);
          if (previous?.service === service)
            return [id, previous.channel] as const;
          readyScreens.current.delete(id);
          const channel = createScreenChannel(id, (message) =>
            screenEvents.current(id, message, channel),
          );
          return [id, channel] as const;
        }),
      ),
    [services],
  );
  screens.current = channels;
  useEffect(() => {
    for (const [id, previous] of screenOwners.current) {
      if (channels.get(id) !== previous.channel) previous.channel.close();
    }
    screenOwners.current = new Map(
      [...services].map(([id, service]) => [
        id,
        { service, channel: channels.get(id)! },
      ]),
    );
  }, [services, channels]);
  useEffect(
    () => () => {
      for (const screen of screens.current.values()) screen.close();
    },
    [],
  );
  useEffect(() => {
    for (const [id, instance] of serviceInstances.current) {
      if (services.get(id) !== instance) instance.close();
    }
    serviceInstances.current = services;
  }, [services]);
  useEffect(
    () => () => {
      for (const instance of serviceInstances.current.values())
        instance.close();
      serviceInstances.current.clear();
    },
    [],
  );
  const sidebarBackends = useMemo(
    () =>
      new Map(
        [...services].map(([id, service]) => [
          id,
          serverSidebarBackend(service, () => sidebarOnline(id)),
        ]),
      ),
    [services, sidebarOnline],
  );
  const refreshSidebarOwner = async (owner: string) => {
    const service = serviceInstances.current.get(owner);
    if (!service) throw new Error("The sidebar server is no longer paired.");
    const value = await fetchServerSummary(
      service.owner,
      new AbortController().signal,
    );
    if (serviceInstances.current.get(owner) !== service)
      throw new Error("The sidebar server connection changed.");
    return { ...value, opened: navigationRef.current[owner]?.opened || null };
  };
  const mergedSelector = useMemo(() => createMergedSidebarSelector(), []);
  const mergedSidebar = useMemo(
    () =>
      selectCombinedSidebar(true, mergedSelector, () =>
        sidebarSources.current.sources(
          servers.map((server) => server.id),
          new Set(
            servers
              .filter((server) => sidebarOnline(server.id))
              .map((server) => server.id),
          ),
        ),
      ),
    [servers, navigation, status, mergedSelector, sidebarOnline],
  );
  const sidebarAliases = useMemo(
    () =>
      Object.fromEntries(
        servers.map((server) => [
          server.id,
          discovery.snapshot?.aliases?.[server.id] ||
            server.alias ||
            (server.id === "local"
              ? DEFAULT_LOCAL_SERVER_ALIAS
              : server.label
                  .replace(/[^a-z]/gi, "")
                  .slice(0, 3)
                  .toUpperCase()),
        ]),
      ),
    [servers, discovery.snapshot?.aliases],
  );
  useEffect(() => {
    if (!manager || (managerTab !== "servers" && managerTab !== "accounts"))
      return;
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
        .catch((error) => {
          if (!stopped)
            setStatus((old) => ({
              ...old,
              [server.id]: summaryFailure(error),
            }));
        })
        .finally(() => controllers.delete(controller));
    }
    return () => {
      stopped = true;
      controllers.forEach((controller) => controller.abort());
    };
  }, [manager, managerTab, servers]);
  useEffect(() => {
    const reread = () => {
      setPaired((old) => shareServers(old, load()));
      setRegistryRevision((value) => value + 1);
    };
    const stored = (event: StorageEvent) => {
      if (event.key === SERVER_REGISTRY_KEY || event.key === null) reread();
    };
    const changed = reread;
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
  screenEvents.current = (id, data: any, channel) => {
    if (screens.current.get(id) !== channel) return;
    if (
      data.kind === "studio-server-activity" &&
      typeof data.busy === "boolean"
    ) {
      // Activity stays with the service; screen controllers remain mounted.
    } else if (data.kind === "studio-open-new-chat") {
      setMobileOpen(false);
      const view = navigationRef.current[id];
      const project = view?.projects.find(
        (row) => row.id === data.projectId || row.path === data.path,
      );
      const directory =
        project?.path ||
        view?.chats.find((row) => row.id === view.opened)?.path ||
        data.path ||
        "";
      setNewChatDialog({
        server: id,
        project: {
          ...project,
          id: project?.id || directory,
          path: directory,
          name: project?.name || directory || "Current folder",
          locations: project?.locations?.length
            ? project.locations
            : [
                {
                  serverId: id,
                  path: directory,
                  projectId: project?.id || directory,
                },
              ],
        },
        folder:
          typeof data.projectFolder === "string"
            ? data.projectFolder
            : undefined,
        // Keep the screen owner's project binding. A folder belongs to that project.
        binding:
          typeof data.projectId === "string" &&
          typeof data.projectServerId === "string"
            ? {
                projectId: data.projectId,
                projectServerId: data.projectServerId,
              }
            : undefined,
      });
    } else if (data.kind === "studio-project-new-chat") {
      const target = serversRef.current.find(
        (server) => server.id === data.target,
      );
      const command =
        target && projectChatCommand(navigationRef.current[id], data);
      if (target && command) send(target.id, command);
    } else if (data.kind === "studio-server-account-dialog") {
      if (typeof data.opened !== "boolean") return;
      if (data.opened) {
        if (managerRef.current) {
          restoreManagerAfterAccountDialog.current = true;
          setManager(false);
        }
      } else if (restoreManagerAfterAccountDialog.current) {
        restoreManagerAfterAccountDialog.current = false;
        setManager(true);
      }
    } else if (data.kind === "studio-server-open-settings") {
      setManagerTab(data.tab === "accounts" ? "accounts" : "servers");
      setManager(true);
    } else if (data.kind === "studio-server-preferences") {
      try {
        const value = parseStudioPreferences(JSON.stringify(data.preferences));
        const next = writePreferenceEdit(
          studioPreferencesStorageKey,
          { ...preferences },
          { ...value },
        );
        setPreferences(next);
        publishPreferences(next);
      } catch {}
    } else if (data.kind === "studio-server-terminal") {
      const value = data;
      if (
        typeof value.opened !== "boolean" ||
        !Number.isFinite(value.height) ||
        !Number.isInteger(value.count) ||
        !Number.isInteger(value.active) ||
        typeof value.canCreate !== "boolean"
      )
        return;
      setTerminals((old) => ({
        ...old,
        [id]: {
          opened: value.opened,
          height: Math.max(0, Math.min(4000, value.height)),
          count: value.count,
          active: value.active,
          canCreate: value.canCreate,
        },
      }));
    } else if (data.kind === "studio-server-toggle-sidebar") {
      if (typeof data.opened === "boolean") setMobileOpen(data.opened);
      else toggleSidebar();
    } else if (data.kind === "studio-server-search") setSearchOpen(true);
    else if (
      data.kind === "studio-server-search-results" &&
      data.requestId === searchRequest.current
    ) {
      setSearchResults((old) => ({
        ...old,
        [id]: { results: data.results || [], error: data.error },
      }));
    } else if (data.kind === "studio-server-navigation") {
      const value = data.navigation as ServerNavigation;
      publishPreferences(preferencesRef.current);
      if (
        !value ||
        !Array.isArray(value.projects) ||
        !Array.isArray(value.chats)
      )
        return;
      if (value.ready) readyScreens.current.add(id);
      applyNavigationRef.current(id, value);
    } else if (data.kind === "studio-server-accounts") {
      if (!Array.isArray(data.accounts) || data.accounts.length > 500) return;
      const accounts: ServerAccount[] = [];
      for (const row of data.accounts) {
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
      data.kind === "studio-server-status" &&
      ["connecting", "live", "offline", "degraded", "schema-mismatch"].includes(
        data.status,
      )
    ) {
      setStatus((old) => ({ ...old, [id]: data.status }));
      if (data.status === "live")
        setLastSeen((old) => ({ ...old, [id]: Date.now() / 1000 }));
    }
  };
  useEffect(
    () =>
      onDesktopNavigate((target) => {
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
  const restoreAccountFocus = () => {
    const identity = restoreAccountActionFocus.current;
    if (!identity || managerTab !== "accounts") return;
    restoreAccountActionFocus.current = null;
    requestAnimationFrame(() => {
      const row = [
        ...document.querySelectorAll<HTMLElement>("[data-account-identity]"),
      ].find((candidate) => candidate.dataset.accountIdentity === identity);
      row
        ?.querySelector<HTMLButtonElement>('[aria-label^="Actions for "]')
        ?.focus({ preventScroll: true });
    });
  };
  const settings = (
    <Modal
      opened={manager}
      transitionProps={{ duration: 0 }}
      onClose={() => setManager(false)}
      size={modalSizes.settings}
      title="Studio settings"
      classNames={{ body: "studio-settings-body" }}
      onEnterTransitionEnd={restoreAccountFocus}
    >
      <div className="studio-settings-panel" data-testid="studio-settings">
        <StudioSettingsLayout
          value={managerTab}
          onChange={(tab) => {
            if (!isStudioSettingsTab(tab)) return;
            setManagerTab(tab);
            if (tab === "accounts" || tab === "servers") return;
            setManager(false);
            send(current, { action: "settings", tab });
          }}
        >
          <Tabs.Panel value="accounts" pt="md">
            {discovery.error && <p role="alert">{discovery.error}</p>}
            <ServerAccountsPanel
              servicesFor={(id) => services.get(id)}
              servers={accountsServers}
              accountsByServer={accountsByServer}
              statuses={{
                ...Object.fromEntries(
                  (discovery.snapshot?.servers || [])
                    .filter(
                      (peer) =>
                        peer.status === "unreachable" ||
                        peer.reachability === "unreachable",
                    )
                    .map((peer) => [peer.id, "offline"]),
                ),
                ...status,
              }}
              startAccount={(id, command) => {
                restoreManagerAfterAccountDialog.current = manager;
                restoreAccountActionFocus.current = `${command.provider}:${(command.email || command.label).toLocaleLowerCase()}`;
                if (servers.some((server) => server.id === id))
                  setManager(false);
                else discovery.addAgain(id);
                send(id, command);
              }}
              accountAction={(id, command) => {
                restoreManagerAfterAccountDialog.current = manager;
                restoreAccountActionFocus.current = `${command.provider}:${(command.email || command.label).toLocaleLowerCase()}`;
                setManager(false);
                send(id, command);
              }}
            />
          </Tabs.Panel>
          <Tabs.Panel value="servers" pt="md">
            {management}
          </Tabs.Panel>
        </StudioSettingsLayout>
      </div>
    </Modal>
  );
  useEffect(() => {
    const narrow = window.matchMedia("(max-width: 760px)");
    let observed: Element | null = null;
    const measure = () => {
      const sidebar = document.querySelector(".multi-server-shell > #sidebar");
      if (sidebar !== observed) {
        if (observed) observer.unobserve(observed);
        if (sidebar) observer.observe(sidebar);
        observed = sidebar;
      }
      const width =
        sidebarHidden || narrow.matches || !sidebar
          ? 0
          : Math.round(sidebar.getBoundingClientRect().width);
      setShellSidebarWidth(width);
      document.documentElement.style.setProperty(
        "--shell-sidebar-width",
        `${width}px`,
      );
      if (width > 0) document.documentElement.dataset.shellSidebar = "true";
      else delete document.documentElement.dataset.shellSidebar;
    };
    const observer = new ResizeObserver(measure);
    measure();
    narrow.addEventListener("change", measure);
    return () => {
      observer.disconnect();
      narrow.removeEventListener("change", measure);
    };
  }, [sidebarHidden, mergedSidebar?.sources.length, servers.length]);
  if (!uiOnly && !paired.length && accountsServers.length === servers.length)
    return (
      <ServerSettingsContext.Provider
        value={{
          server: localServer(),
          panel: management,
          settingsOpen: manager,
          settingsTab: managerTab,
          setSettingsOpen: setManager,
          setSettingsTab: (tab) => {
            if (isStudioSettingsTab(tab)) setManagerTab(tab);
          },
        }}
      >
        <App />
      </ServerSettingsContext.Provider>
    );
  const terminal = terminals[current];
  const terminalLayout =
    terminal && shellSidebarWidth > 0
      ? terminal.opened
        ? "open"
        : "closed"
      : undefined;
  return (
    <div
      className={`multi-server-shell ${sidebarHidden ? "server-sidebar-hidden" : ""}`}
      data-navigation="combined"
      data-shell-terminal={terminalLayout}
      style={
        {
          "--shell-sidebar-width": `${shellSidebarWidth}px`,
          "--shell-terminal-height": `${terminal?.height || 0}px`,
        } as CSSProperties
      }
    >
      <>
        <Button
          className="server-mobile-toggle"
          onClick={() => setMobileOpen(!mobileOpen)}
          aria-expanded={mobileOpen}
        >
          Servers and chats
        </Button>
        {mergedSidebar?.sources.length ? (
          <CombinedServerSidebar
            model={mergedSidebar}
            backends={sidebarBackends}
            online={sidebarOnline}
            refreshOwner={refreshSidebarOwner}
            executeCommand={executeCommand}
            aliases={sidebarAliases}
            current={current}
            navigation={navigation}
            send={send}
            applyNavigation={(owner, value) =>
              applyNavigationRef.current(owner, value)
            }
            mobile={mobileOpen}
            hidden={sidebarHidden}
            close={() => {
              setMobileOpen(false);
              setSidebarHidden(true);
            }}
            settings={openManager}
            search={() => setSearchOpen(true)}
            notify={setFailure}
            notice={failure}
            hideOldChatsThreshold={preferences.hideOldChatsThreshold}
          />
        ) : (
          <aside className="server-sidebar" aria-label="Servers and projects">
            <header>
              <strong>LAYR Studio</strong>
              <Button
                variant="subtle"
                aria-label="Studio settings"
                onClick={openManager}
              >
                Settings
              </Button>
            </header>
            <p role="status">
              {servers.length
                ? "Loading chats..."
                : "Pair a server to open its projects and chats."}
            </p>
            {failure && <p role="alert">{failure}</p>}
          </aside>
        )}
      </>
      {terminalLayout === "closed" && terminal && (
        <div className="shell-terminal-bar">
          <button
            type="button"
            className="shell-terminal-toggle"
            aria-expanded={false}
            aria-label="Show terminals"
            onClick={() =>
              send(current, { action: "terminal", operation: "toggle" })
            }
          >
            <TerminalIcon size={15} />
            <strong>Terminals</strong>
            {terminal.count > 0 && (
              <span className="terminal-dock-count">{terminal.count}</span>
            )}
            <ChevronUp size={14} />
          </button>
          <ActionIcon
            variant="filled"
            size={28}
            aria-label="New terminal"
            title="New terminal"
            disabled={!terminal.canCreate}
            onClick={() =>
              send(current, { action: "terminal", operation: "new" })
            }
          >
            <Plus size={13} />
          </ActionIcon>
        </div>
      )}
      <main className="server-views">
        {!servers.length && (
          <div className="startup">
            <p>No server is paired.</p>
            <Button onClick={openManager}>Open Settings</Button>
          </div>
        )}
        {servers.map((server) => {
          const service = services.get(server.id)!;
          const channel = channels.get(server.id)!;
          return (
            <ServerServicesProvider
              key={JSON.stringify([
                server.id,
                server.origin,
                server.credentialId,
              ])}
              services={service}
            >
              <ScreenContext.Provider value={channel}>
                <App active={current === server.id} />
              </ScreenContext.Provider>
            </ServerServicesProvider>
          );
        })}
      </main>
      {settings}
      <Modal
        opened={!!newChatDialog}
        closeOnEscape={false}
        onClose={() => setNewChatDialog(null)}
        title="New chat"
        aria-label="New chat"
        size="lg"
      >
        {newChatDialog && (
          <ProjectServerCards
            project={newChatDialog.project}
            servers={projectServerChoices(
              servers,
              status,
              Object.fromEntries(
                (discovery.snapshot?.servers || []).map((server) => [
                  server.id,
                  server.reachability,
                ]),
              ),
            ).map((row) => ({
              ...row,
              system: navigation[row.id]?.system,
            }))}
            lastServer={newChatDialog.server}
            onCancel={() => setNewChatDialog(null)}
            onAdd={(server) => {
              send(newChatDialog.server, {
                action: "project-folders",
                projectId: newChatDialog.project.id,
                server,
              });
              setNewChatDialog(null);
            }}
            onChoose={(location, settings) => {
              const origin = location.serverId === newChatDialog.server;
              send(location.serverId, {
                action: "new-chat",
                path: location.path,
                settings,
                ...(origin && newChatDialog.folder
                  ? { folder: newChatDialog.folder }
                  : {}),
                ...(origin && newChatDialog.binding
                  ? newChatDialog.binding
                  : newChatDialog.project.homeServerId
                    ? {
                        projectId: newChatDialog.project.id,
                        projectServerId: newChatDialog.project.homeServerId,
                      }
                    : {}),
              });
              setNewChatDialog(null);
            }}
          />
        )}
      </Modal>
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
            for (const [id, service] of services) {
              void service.api
                .get("/api/search", { query: { q: searchQuery.trim() } })
                .then(
                  (value) => {
                    if (
                      searchRequest.current === requestId &&
                      serviceInstances.current.get(id) === service
                    )
                      setSearchResults((old) => ({
                        ...old,
                        [id]: { results: value.results },
                      }));
                  },
                  (error) => {
                    if (
                      searchRequest.current === requestId &&
                      serviceInstances.current.get(id) === service
                    )
                      setSearchResults((old) => ({
                        ...old,
                        [id]: { results: [], error: String(error) },
                      }));
                  },
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
                <p role="alert">{searchResults[server.id]?.error}</p>
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
