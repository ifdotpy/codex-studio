import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { Snapshot } from "../types";
import type { ServerNavigation } from "./navigation";

const hooks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
}));
vi.mock("react", async (importOriginal) => ({
  ...(await importOriginal<typeof import("react")>()),
  useEffect: (effect: () => void | (() => void)) => hooks.effects.push(effect),
  useRef: (current: unknown) => ({ current }),
}));
vi.mock("./environment", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./environment")>()),
  serverViewId: "remote",
  serverParentOrigin: "https://parent.example",
  serverStorageName: (name: string) => `${name}:server:remote`,
}));
vi.mock("./transport", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./transport")>()),
  apiOrigin: () => "https://remote.example",
}));
const stopConnection = vi.hoisted(() => vi.fn());
vi.mock("../sync/resourceEvents", () => ({
  watchResourceConnection: () => stopConnection,
}));

import { useServerFrame } from "./frameBridge";
import { preferenceEvent } from "../sync/uiPreferenceMerge";
import { writePreferenceEdit } from "../sync/uiPreferenceStore";
import { defaultStudioPreferences } from "../studioPreferences";

class TestStorageEvent extends Event {
  readonly key: string | null;
  constructor(type: string, init: { key?: string | null } = {}) {
    super(type);
    this.key = init.key ?? null;
  }
}

let surface: EventTarget & {
  parent: { postMessage: ReturnType<typeof vi.fn> };
};
let records: Map<string, string>;
let cleanup: (() => void)[];
beforeEach(() => {
  hooks.effects = [];
  cleanup = [];
  records = new Map();
  surface = Object.assign(new EventTarget(), {
    parent: { postMessage: vi.fn() },
  });
  vi.stubGlobal("window", surface);
  vi.stubGlobal("StorageEvent", TestStorageEvent);
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => records.get(key) ?? null,
    setItem: (key: string, value: string) => records.set(key, value),
  });
  stopConnection.mockClear();
});
afterEach(() => {
  cleanup.forEach((stop) => stop());
  vi.unstubAllGlobals();
});

function mount(
  data: Snapshot | null = {
    token: "session-token",
    stateDir: "/same/state",
    threads: [
      {
        id: "lead",
        name: "Real chat",
        source: "managed",
        isLead: true,
        cwd: "/work",
      },
    ],
    chats: [],
    nodes: [],
    edges: [],
    runtime: {
      agents: [],
      rooms: [],
      tasks: [],
      monitors: [],
      complaints: [],
      requests: [],
      rules: [],
      projects: [],
      peerTeams: [],
      events: [],
      work: [],
    },
  },
) {
  const run = vi.fn();
  useServerFrame(data, "lead", "", run, new Set(["lead"]), []);
  for (const effect of hooks.effects) {
    const stop = effect();
    if (stop) cleanup.push(stop);
  }
  return run;
}

type NavigationMessage = {
  kind: string;
  serverId: string;
  navigation: ServerNavigation;
};
function navigationMessages(): [NavigationMessage, string][] {
  return surface.parent.postMessage.mock.calls
    .filter(([message]) => message.kind === "studio-server-navigation")
    .map(([message, origin]): [NavigationMessage, string] => [message, origin]);
}

function message(
  data: unknown,
  origin = "https://parent.example",
  source: unknown = surface.parent,
) {
  surface.dispatchEvent(
    Object.assign(new Event("message"), { data, origin, source }),
  );
}

it("publishes a real scoped snapshot and republishes after a saved preference edit", () => {
  records.set(
    "codex-project-compact:/same/state",
    JSON.stringify({ "/work": false }),
  );
  records.set(
    ":server:other:codex-project-compact:/same/state",
    JSON.stringify({ "/work": false }),
  );
  mount();
  expect(navigationMessages()).toHaveLength(1);
  const [initial, origin] = navigationMessages()[0];
  expect(origin).toBe("https://parent.example");
  expect(initial.serverId).toBe("remote");
  expect(initial.navigation.sidebar?.threads[0].name).toBe("Real chat");
  expect(initial.navigation.sidebar?.compact).toEqual({});
  expect(initial.navigation.sidebar?.unread).toEqual(["lead"]);
  expect(JSON.stringify(initial.navigation.sidebar)).not.toContain(
    "session-token",
  );

  writePreferenceEdit(
    "codex-project-compact:/same/state",
    {},
    { "/work": true },
  );
  expect(navigationMessages()).toHaveLength(2);
  expect(navigationMessages().at(-1)?.[0].navigation.sidebar?.compact).toEqual({
    "/work": true,
  });
  const folder = JSON.stringify(["/work", "folder", "nested"]);
  writePreferenceEdit("codex-project-tree:/same/state", {}, { [folder]: true });
  expect(navigationMessages()).toHaveLength(3);
  expect(
    navigationMessages().at(-1)?.[0].navigation.sidebar?.collapsed,
  ).toEqual({ [folder]: true });
  surface.dispatchEvent(
    new CustomEvent(preferenceEvent, {
      detail: "codex-project-tree:/other/state",
    }),
  );
  surface.dispatchEvent(
    new TestStorageEvent("storage", {
      key: ":server:other:codex-project-tree:/same/state",
    }),
  );
  expect(navigationMessages()).toHaveLength(3);
});

it("republishes saved order and tree changes from scoped or materialized storage events", () => {
  mount();
  records.set(
    ":server:remote:codex-sidebar-order:/same/state",
    JSON.stringify({ projects: ["/work"] }),
  );
  surface.dispatchEvent(
    new TestStorageEvent("storage", {
      key: ":server:remote:codex-sidebar-order:/same/state",
    }),
  );
  expect(navigationMessages()).toHaveLength(2);
  expect(
    navigationMessages().at(-1)?.[0].navigation.sidebar?.savedOrder,
  ).toEqual({ projects: ["/work"] });
  expect(
    navigationMessages().at(-1)?.[0].navigation.sidebar?.sidebarOrder,
  ).toBeUndefined();
  records.set(
    ":server:remote:codex-project-tree:/same/state",
    JSON.stringify({ "/work": true }),
  );
  surface.dispatchEvent(
    new TestStorageEvent("storage", { key: "codex-project-tree:/same/state" }),
  );
  expect(navigationMessages()).toHaveLength(3);
  expect(
    navigationMessages().at(-1)?.[0].navigation.sidebar?.collapsed,
  ).toEqual({ "/work": true });
  records.clear();
  surface.dispatchEvent(new TestStorageEvent("storage"));
  expect(navigationMessages()).toHaveLength(4);
  expect(
    navigationMessages().at(-1)?.[0].navigation.sidebar?.savedOrder,
  ).toEqual({});
});

it("keeps command and preference source, origin and server checks", () => {
  const run = mount();
  const command = {
    kind: "studio-server-command",
    serverId: "remote",
    command: { action: "open", id: "lead" },
  };
  message(command, "https://untrusted.example");
  message(command, "https://parent.example", {});
  message({ ...command, serverId: "other" });
  const preference = {
    kind: "studio-server-preferences",
    serverId: "remote",
    preferences: { ...defaultStudioPreferences, theme: "dark" },
  };
  message(preference, "https://untrusted.example");
  message(preference, "https://parent.example", {});
  message({ ...preference, serverId: "other" });
  expect(run).not.toHaveBeenCalled();
  expect(records.has("codex-studio-preferences-v1")).toBe(false);
  message(command);
  expect(run).toHaveBeenCalledExactlyOnceWith({ action: "open", id: "lead" });
  message(preference);
  expect(
    JSON.parse(records.get("codex-studio-preferences-v1") || "null").theme,
  ).toBe("dark");
  expect(navigationMessages()).toHaveLength(1);
});

it("removes preference and storage listeners when the frame effect stops", () => {
  mount();
  cleanup.forEach((stop) => stop());
  cleanup = [];
  surface.dispatchEvent(
    new CustomEvent(preferenceEvent, {
      detail: "codex-project-tree:/same/state",
    }),
  );
  surface.dispatchEvent(
    new TestStorageEvent("storage", { key: "codex-project-tree:/same/state" }),
  );
  expect(navigationMessages()).toHaveLength(1);
  expect(stopConnection).toHaveBeenCalledOnce();
});

it("keeps navigation without sidebar data until a snapshot is available", () => {
  mount(null);
  surface.dispatchEvent(
    new CustomEvent(preferenceEvent, {
      detail: "codex-project-tree:/same/state",
    }),
  );
  expect(navigationMessages()).toHaveLength(1);
  expect(navigationMessages()[0][0].navigation).toMatchObject({ ready: false });
  expect(navigationMessages()[0][0].navigation).not.toHaveProperty("sidebar");
});
