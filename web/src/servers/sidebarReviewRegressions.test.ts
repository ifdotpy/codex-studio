import { expect, it, vi } from "vitest";
import { execFileSync } from "node:child_process";
import {
  mergeSidebar,
  createMergedSidebarSelector,
  type SidebarSource,
  selectCombinedSidebar,
} from "./mergedSidebar";
import {
  mergedSidebarPreferences,
  sidebarOrderWrite,
} from "./mergedSidebarPreferences";
import {
  localSidebarBackend,
  type SidebarBackend,
} from "../components/sidebar/services";
import type { Agent } from "../types";

function source(id: string, count = 1, directories = false): SidebarSource {
  const path = "/" + id;
  return {
    id,
    online: true,
    sidebar: {
      stateDir: "/same/state",
      threads: Array.from(
        { length: count },
        (_, i) =>
          ({
            id: "chat-" + i,
            cwd: directories ? path + "/" + i : path,
            source: "managed",
            isLead: true,
            name: "Chat " + i,
          }) as Agent,
      ),
      projects: [{ id: "p-" + id, path, name: id, folders: [] }],
      rooms: [],
      peerTeams: [],
      savedOrder: {},
      compact: {},
      collapsed: { [path]: true },
      indicators: [],
      unread: [],
      sidebarOrder: {
        revision: 1,
        groups: {
          [JSON.stringify(["items", path, null])]: Array.from(
            { length: count },
            (_, i) => "chat-" + i,
          ),
        },
      },
      projectOrganizationVersion: 1,
      peerTeamsVersion: 1,
    },
  };
}
function preferences(a: SidebarSource, b?: SidebarSource) {
  const sources = b ? [a, b] : [a];
  const writes = vi.fn();
  const backends = new Map<string, SidebarBackend>(
    sources.map((s) => [
      s.id,
      {
        ...localSidebarBackend,
        ownerId: s.id,
        editPreference(key, previous, next) {
          writes(s.id, key, previous, next);
          return next;
        },
      },
    ]),
  );
  const model = mergeSidebar(sources, 1, true);
  return { model, writes, cache: mergedSidebarPreferences(model, backends) };
}
it("an explicit shared heading edit writes only the home owner", () => {
  const a = source("local"),
    b = source("remote");
  b.sidebar.projects[0].projectAliases = [
    { serverId: "local", projectId: "p-local", name: "local" },
  ];
  const { model, writes, cache } = preferences(a, b);
  const key = "codex-project-tree:" + model.data.stateDir;
  const previous = cache.saved<Record<string, boolean>>(key, {});
  cache.editPreference(key, previous, {
    ...previous,
    [model.projectKey("remote", "/remote")]: false,
  });
  expect(writes.mock.calls.map((call) => call[0])).toEqual(["local"]);
});
it("an unregistered directory has an original collapse key", () => {
  const a = source("local");
  a.sidebar.projects = [];
  a.sidebar.collapsed = {};
  const { model, writes, cache } = preferences(a);
  const key = "codex-project-tree:" + model.data.stateDir;
  const previous = cache.saved<Record<string, boolean>>(key, {});
  const field = model.projectKey("local", "/local");
  expect(
    cache.editPreference(key, previous, { ...previous, [field]: true })[field],
  ).toBe(true);
  expect(writes).toHaveBeenCalledExactlyOnceWith(
    "local",
    "codex-project-tree:/same/state",
    {},
    { "/local": true },
  );
});
it("a reorder with 4000 chats on each server passes the real backend validator", () => {
  const a = source("local", 4000),
    b = source("remote", 4000);
  b.sidebar.projects[0].projectAliases = [
    { serverId: "local", projectId: "p-local", name: "local" },
  ];
  const model = mergeSidebar([a, b]);
  const group = JSON.stringify([
    "items",
    model.projectKey("local", "/local"),
    null,
  ]);
  const desired = [...model.order[group]].reverse();
  const payload = sidebarOrderWrite(model, "local", {
    ...model.order,
    [group]: desired,
  });
  const code = `import json,sys
from types import SimpleNamespace
from codex_project_folders import reorder_sidebar
class Passed(Exception): pass
class Lock:
 def __enter__(self): raise Passed()
 def __exit__(self,*args): pass
try: reorder_sidebar(SimpleNamespace(lock=Lock()),json.load(sys.stdin))
except Passed: print("validated")`;
  console.info(
    "Order validator IDs:",
    Object.values(payload.groups).reduce(
      (count, items) => count + items.length,
      0,
    ),
  );
  expect(
    execFileSync(
      "python3",
      ["../scripts/codex_python.py", "--exec", "-c", code],
      {
        input: JSON.stringify({
          action: "reorder",
          request_id: "review-limit",
          ...payload,
        }),
        encoding: "utf8",
      },
    ).trim(),
  ).toBe("validated");
  a.sidebar.sidebarOrder = { revision: 2, groups: payload.groups };
  expect(mergeSidebar([a, b]).order[group]).toEqual(desired);
});
it.each([false, true])(
  "merges 6000 chat directories within 500ms (registered: %s)",
  (registered) => {
    const a = source("local", 6000, true);
    if (registered)
      a.sidebar.projects = a.sidebar.threads.map((chat) => ({
        id: chat.cwd!,
        path: chat.cwd!,
        folders: [],
      }));
    const select = createMergedSidebarSelector();
    const times = [];
    for (let i = 0; i < 3; i++) {
      const start = performance.now();
      const model = select([{ ...a, sidebar: { ...a.sidebar } }]);
      expect(model.data.runtime.projects).toHaveLength(
        registered ? 6000 : 6001,
      );
      times.push(performance.now() - start);
    }
    console.info("Merge 6000 directories (ms):", times);
    expect(times.sort((a, b) => a - b)[1]).toBeLessThan(500);
  },
);

it("classic mode does not request sources or construct the merged model", () => {
  const select = vi.fn(createMergedSidebarSelector());
  const sources = vi.fn(() => [source("local")]);
  expect(selectCombinedSidebar(false, select, sources)).toBeUndefined();
  expect(select).not.toHaveBeenCalled();
  expect(sources).not.toHaveBeenCalled();
  expect(selectCombinedSidebar(true, select, sources)?.sources).toHaveLength(1);
  expect(select).toHaveBeenCalledOnce();
  expect(sources).toHaveBeenCalledOnce();
});

it("an explicit reorder converts legacy combined ranks without changing unknown native IDs", () => {
  const a = source("local", 2),
    b = source("remote", 2);
  b.sidebar.projects[0].projectAliases = [
    { serverId: "local", projectId: "p-local", name: "local" },
  ];
  const initial = mergeSidebar([a, b]);
  const group = JSON.stringify([
    "items",
    initial.projectKey("local", "/local"),
    null,
  ]);
  const oldOrder = [...initial.order[group]].reverse();
  const legacyKey = JSON.stringify(["combined-sidebar", group]);
  a.sidebar.sidebarOrder!.groups![legacyKey] = oldOrder;
  const nativeGroup = JSON.stringify(["items", "/local", null]);
  a.sidebar.sidebarOrder!.groups![nativeGroup].push("unknown-native-id");
  const model = mergeSidebar([a, b]);
  expect(model.order[group].slice(0, 4)).toEqual(oldOrder);
  const desired = [...model.order[group]].reverse();
  const payload = sidebarOrderWrite(model, "local", {
    ...model.order,
    [group]: desired,
  });
  expect(payload.groups[legacyKey]).toBeUndefined();
  expect(payload.groups[nativeGroup]).toContain("unknown-native-id");
  a.sidebar.sidebarOrder = { revision: 2, groups: payload.groups };
  expect(mergeSidebar([a, b]).order[group]).toEqual(desired);
});
