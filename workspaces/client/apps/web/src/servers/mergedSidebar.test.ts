import { decodeSidebarRanks } from "./sidebarOrderRanks";
import { describe, expect, it } from "vitest";
import type { Agent, Project } from "../types";
import {
  localSidebarBackend,
  type SidebarBackend,
} from "../components/sidebar/services";
import {
  createSidebarCatalogSelector,
  createSidebarSearchSelector,
  itemGroup,
} from "../components/sidebar/catalog";
import {
  createMergedSidebarSelector,
  createSidebarSourceStore,
  mergeSidebar,
  type SidebarSource,
} from "./mergedSidebar";
import {
  mergedSidebarPreferences,
  sidebarOrderWrite,
} from "./mergedSidebarPreferences";

const path = "/same/project";
const scope = "/same/state";
const treeKey = (id: string) => JSON.stringify([path, "folder", id]);
const agent = (id: string, patch: Partial<Agent> = {}): Agent => ({
  id,
  name: id,
  cwd: path,
  source: "managed",
  isLead: true,
  rootId: id,
  created: 1,
  ...patch,
});
function source(
  id: string,
  projects: Project[] = [{ id: path, path, name: id, folders: [] }],
): SidebarSource {
  return {
    id,
    online: true,
    sidebar: {
      stateDir: scope,
      projects,
      threads: [
        agent("same", { pinned: true }),
        agent("child", { isLead: false, parentId: "same", rootId: "same" }),
      ],
      rooms: [
        {
          id: "room",
          name: "Shared",
          projectPath: path,
          members: ["same"],
          radio: {
            direct: true,
            active: null,
            error: null,
            next: [],
            revision: 0,
            status: "idle",
            speaker: null,
            teamId: "",
          },
        },
      ],
      peerTeams: [],
      peerTeamsVersion: 1,
      projectOrganizationVersion: 1,
      sidebarOrder: {
        revision: 3,
        groups: { projects: [path], [itemGroup(path)]: ["same", "missing"] },
      },
      savedOrder: {},
      compact: { [path]: false },
      collapsed: {},
      indicators: [["same", { kind: "answer", label: "Needs an answer" }]],
      unread: ["same"],
    },
  };
}
function backend(value: SidebarSource) {
  const values = new Map<string, string>();
  const writes: string[] = [];
  const listeners = new Map<string, Set<() => void>>();
  const storage = {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, data: string) => {
      values.set(key, data);
      writes.push(key);
    },
    removeItem: (key: string) => {
      values.delete(key);
      writes.push(key);
    },
  };
  const result: SidebarBackend = {
    ...localSidebarBackend,
    ownerId: value.id,
    storage,
    saved<T>(key: string, fallback: T): T {
      return values.has(key) ? JSON.parse(values.get(key)!) : fallback;
    },
    save(key, data) {
      storage.setItem(key, JSON.stringify(data));
    },
    editPreference(key, _previous, next) {
      storage.setItem(key, JSON.stringify(next));
      listeners.get(key)?.forEach((listener) => listener());
      return next;
    },
    subscribePreference(key, update) {
      const callbacks = listeners.get(key) || new Set();
      callbacks.add(update);
      listeners.set(key, callbacks);
      return () => {
        callbacks.delete(update);
      };
    },
  };
  return { backend: result, values, writes, listeners };
}

describe("complete multi-server sidebar projection", () => {
  it("keeps render identities stable while a paired source is still loading", () => {
    const local = source("local");
    const remote = source("remote");
    const select = createMergedSidebarSelector();
    const first = select([local]);
    const complete = select([local, remote]);
    expect(first.data.stateDir).toBe(complete.data.stateDir);
    expect(first.data.threads[0].id).toBe(complete.data.threads[0].id);
    expect(first.data.runtime.projects[0].path).toBe(
      complete.data.runtime.projects[0].path,
    );
  });

  it("keeps an unavailable home explicit instead of routing metadata to an alias server", () => {
    const remote = source("remote", [
      {
        id: path,
        path,
        name: "Remote folder",
        projectAliases: [
          {
            serverId: "unpaired-home",
            projectId: "project:home",
            name: "Home",
          },
        ],
      },
    ]);
    const model = mergeSidebar([remote], 1, true);
    const project = model.data.runtime.projects[0];
    expect(model.projectReferences.get(project.path!)?.owner).toBe(
      "unpaired-home",
    );
    expect(model.sourceById.has("unpaired-home")).toBe(false);
    expect(model.wireProject("remote", project.path!)).toBe(path);
  });

  it("preserves single-server identities, objects, storage and null order semantics", () => {
    const local = source("local");
    local.sidebar.sidebarOrder = { revision: 0, groups: null };
    local.sidebar.savedOrder = { projects: [path] };
    const model = mergeSidebar([local]);
    const original = backend(local);
    expect(model.data.threads).toEqual(local.sidebar.threads);
    expect(model.data.threads[0]).toBe(local.sidebar.threads[0]);
    expect(model.data.runtime.projects[0]).toBe(local.sidebar.projects[0]);
    expect(model.data.runtime.rooms[0]).toBe(local.sidebar.rooms[0]);
    expect(model.data.runtime.sidebarOrder).toBe(local.sidebar.sidebarOrder);
    expect(model.data.stateDir).toBe(scope);
    expect(
      mergedSidebarPreferences(model, new Map([["local", original.backend]])),
    ).toBe(original.backend);
    expect(original.writes).toEqual([]);
  });

  it("separates overlapping chat, worker, room, folder and team IDs", () => {
    const a = source("local");
    const b = source("remote");
    for (const value of [a, b]) {
      value.sidebar.projects[0].folders = [{ id: "folder", name: "Notes" }];
      value.sidebar.threads[0].projectFolder = "folder";
      value.sidebar.peerTeams = [
        { id: "team", name: value.id, projectPath: path, members: ["same"] },
      ];
    }
    const model = mergeSidebar([a, b]);
    expect(model.data.runtime.projects).toHaveLength(2);
    expect(new Set(model.data.threads.map((row) => row.id)).size).toBe(4);
    expect(new Set(model.data.runtime.rooms.map((row) => row.id)).size).toBe(2);
    expect(
      new Set(model.data.runtime.peerTeams.map((row) => row.id)).size,
    ).toBe(2);
    for (const value of [a, b]) {
      const lead = model.data.threads.find(
        (row) => row.id === model.chatKey(value.id, "same"),
      )!;
      const worker = model.data.threads.find(
        (row) => row.id === model.chatKey(value.id, "child"),
      )!;
      expect(worker.parentId).toBe(lead.id);
      expect(worker.rootId).toBe(lead.id);
      expect(lead.serverId).toBe(value.id);
      expect(model.folderReferences.get(lead.projectFolder!)).toEqual({
        owner: value.id,
        id: "folder",
        path,
      });
      expect(model.chatReferences.get(lead.id)).toEqual({
        owner: value.id,
        id: "same",
        path,
      });
      expect(model.indicators.get(lead.id)?.kind).toBe("answer");
      expect(model.unread.has(lead.id)).toBe(true);
    }
    expect(() => mergeSidebar([a, a])).toThrow("Duplicate sidebar source");
  });

  it("uses one home heading and keeps conflicting remote branches, independent of arrival order", () => {
    const home = source("local", [
      {
        id: "project:shared",
        path: "project:shared",
        name: "Home project",
        homeServerId: "home-id",
        locations: [
          { serverId: "home-id", path, projectId: path },
          {
            serverId: "remote",
            path: "/remote/path",
            projectId: "/remote/path",
          },
        ],
        folders: [
          { id: "same", name: "Home folder" },
          { id: "nested", name: "Child", parentId: "same" },
          { id: "compatible", name: "Shared folder" },
        ],
      },
    ]);
    const remote = source("remote", [
      {
        id: "/remote/path",
        path: "/remote/path",
        name: "Remote project",
        homeServerId: "remote",
        projectAliases: [
          {
            serverId: "home-id",
            projectId: "project:shared",
            name: "Home project",
          },
        ],
        folders: [
          { id: "same", name: "Remote folder" },
          { id: "nested", name: "Child", parentId: "same" },
          { id: "compatible", name: "Shared folder" },
        ],
      },
    ]);
    remote.sidebar.threads = [
      agent("same", {
        cwd: "/remote/path",
        projectId: "project:shared",
        projectServerId: "home-id",
        projectFolder: "nested",
      }),
    ];
    remote.sidebar.peerTeams = [
      {
        id: "team",
        name: "Remote team",
        projectPath: "/remote/path",
        members: ["same"],
      },
    ];
    const model = mergeSidebar([remote, home]);
    expect(model.data.runtime.projects).toHaveLength(1);
    const project = model.data.runtime.projects[0];
    expect(project.name).toBe("Home project");
    expect(project.folders?.map((row) => row.name)).toEqual([
      "Home folder",
      "Child",
      "Shared folder",
      "Remote folder",
      "Child",
    ]);
    const folder = model.folderReferences.get(
      model.data.threads[0].projectFolder!,
    );
    expect(folder?.owner).toBe("remote");
    const nested = project.folders!.find(
      (row) => row.id === model.data.threads[0].projectFolder,
    );
    expect(model.folderReferences.get(nested!.parentId!)?.owner).toBe("remote");
    expect(model.data.threads.every((row) => row.cwd === project.path)).toBe(
      true,
    );
    expect(model.data.runtime.peerTeams[0].projectPath).toBe(project.path);
    expect(model.wireProject("remote", project.path!)).toBe("/remote/path");
    expect(model.wireProject("local", project.path!)).toBe("project:shared");
  });

  it("retains archived rows, chat tails, peer membership and original indicators in the classic catalog", () => {
    const local = source("local");
    const remote = source("remote");
    remote.sidebar.threads[0] = agent("same", {
      archived: true,
      tail: "find this tail",
      projectFolder: "folder",
    });
    remote.sidebar.peerTeams = [
      { id: "team", projectPath: path, members: ["same"] },
    ];
    const model = mergeSidebar([local, remote]);
    const catalog = createSidebarCatalogSelector()(
      model.data.threads,
      {},
      model.data.runtime.peerTeams,
      model.order,
    );
    expect(catalog.agents).toHaveLength(2);
    expect(catalog.byTeam.get(model.chatKey("remote", "same"))?.id).toBe(
      model.teamKey("remote", "team"),
    );
    const found = createSidebarSearchSelector()(
      catalog.agents,
      model,
      "find this tail",
      (row) => row.tail || "",
    );
    expect(found.map((row) => row.id)).toEqual([
      model.chatKey("remote", "same"),
    ]);
    expect(found[0].archived).toBe(true);
  });

  it("retains offline rows on summary updates and shares unchanged projected rows", () => {
    const local = source("local");
    const remote = source("remote");
    const store = createSidebarSourceStore();
    store.update("local", local.sidebar);
    store.update("remote", remote.sidebar);
    store.update("remote", undefined);
    const sources = store.sources(["local", "remote"], new Set(["local"]));
    expect(sources[1].sidebar).toBe(remote.sidebar);
    expect(sources[1].online).toBe(false);
    const select = createMergedSidebarSelector();
    const before = select(sources);
    expect(select(sources)).toBe(before);
    const next = select([
      {
        ...sources[0],
        sidebar: {
          ...local.sidebar,
          projects: [{ ...local.sidebar.projects[0], name: "Renamed" }],
        },
      },
      sources[1],
    ]);
    expect(next.data.threads[0]).toBe(before.data.threads[0]);
    expect(next.data.runtime.sidebarOrder!.revision).toBeGreaterThan(
      before.data.runtime.sidebarOrder!.revision,
    );
    store.forget("remote");
    expect(
      store.sources(["local", "remote"], new Set()).map((row) => row.id),
    ).toEqual(["local"]);
  });
});

describe("original saved sidebar state", () => {
  it.each([undefined, false, true])(
    "ignores saved project collapse state in every source (remote: %s)",
    (remoteValue) => {
      const local = source("local");
      const remote = source("remote", [
        {
          id: path,
          path,
          name: "Remote",
          projectAliases: [
            { serverId: "local", projectId: path, name: "local" },
          ],
        },
      ]);
      local.sidebar.collapsed = { [path]: true };
      remote.sidebar.collapsed =
        remoteValue === undefined ? {} : { [path]: remoteValue };
      const model = mergeSidebar([local, remote]);
      const heading = model.projectKey("local", path);
      expect(model.projectKey("remote", path)).toBe(heading);
      expect(model.collapsed[heading]).toBeUndefined();
      local.sidebar.collapsed = {};
      remote.sidebar.collapsed = { [path]: true };
      expect(mergeSidebar([local, remote]).collapsed[heading]).toBeUndefined();
    },
  );

  it("keeps a compatible remote folder open when its home folder is collapsed", () => {
    const local = source("local");
    const remote = source("remote", [
      {
        id: path,
        path,
        name: "Remote",
        projectAliases: [{ serverId: "local", projectId: path, name: "local" }],
      },
    ]);
    for (const value of [local, remote])
      value.sidebar.projects[0].folders = [{ id: "shared", name: "Notes" }];
    local.sidebar.collapsed = { [treeKey("shared")]: true };
    const model = mergeSidebar([local, remote]);
    expect(model.mapTreeKey("local", treeKey("shared"))).toBe(
      model.mapTreeKey("remote", treeKey("shared")),
    );
    expect(model.collapsed[model.mapTreeKey("local", treeKey("shared"))]).toBe(
      false,
    );
    expect(local.sidebar.collapsed).toEqual({ [treeKey("shared")]: true });
    expect(remote.sidebar.collapsed).toEqual({});
  });

  it("uses the owner's saved compact preference despite its stale snapshot", () => {
    const local = source("local");
    const remote = source("remote");
    const a = backend(local);
    const b = backend(remote);
    a.values.set(
      `codex-project-tree:${scope}`,
      JSON.stringify({ [path]: true }),
    );
    b.values.set(
      `codex-project-compact:${scope}`,
      JSON.stringify({ [path]: true }),
    );
    const model = mergeSidebar([local, remote]);
    const cache = mergedSidebarPreferences(
      model,
      new Map([
        ["local", a.backend],
        ["remote", b.backend],
      ]),
    );
    expect(
      cache.saved<Record<string, boolean>>(
        `codex-project-tree:${model.data.stateDir}`,
        {},
      )[model.projectKey("local", path)],
    ).toBeUndefined();
    expect(
      cache.saved<Record<string, boolean>>(
        `codex-project-compact:${model.data.stateDir}`,
        {},
      )[model.projectKey("remote", path)],
    ).toBe(true);
    expect(a.writes).toEqual([]);
    expect(b.writes).toEqual([]);
    const compactKey = `codex-project-compact:${model.data.stateDir}`;
    const field = model.projectKey("remote", path);
    cache.editPreference(compactKey, { [field]: true }, { [field]: false });
    expect(b.values.get(`codex-project-compact:${scope}`)).toBe(
      JSON.stringify({ [path]: false }),
    );
    const reopened = mergedSidebarPreferences(
      model,
      new Map([
        ["local", a.backend],
        ["remote", b.backend],
      ]),
    );
    expect(reopened.saved<Record<string, boolean>>(compactKey, {})[field]).toBe(
      false,
    );
  });

  it("reads original namespaces without writing and ignores project collapse with shared compact visibility", () => {
    const local = source("local");
    const remote = source("remote", [
      {
        id: path,
        path,
        name: "Remote",
        projectAliases: [{ serverId: "local", projectId: path, name: "local" }],
      },
    ]);
    local.sidebar.compact = { [path]: true };
    local.sidebar.collapsed = { [path]: true };
    remote.sidebar.compact = { [path]: false };
    const a = backend(local);
    const b = backend(remote);
    const model = mergeSidebar([local, remote]);
    const cache = mergedSidebarPreferences(
      model,
      new Map([
        ["local", a.backend],
        ["remote", b.backend],
      ]),
    );
    expect(
      cache.saved(`codex-project-compact:${model.data.stateDir}`, {}),
    ).toEqual({ [model.projectKey("local", path)]: false });
    expect(
      cache.saved(`codex-project-tree:${model.data.stateDir}`, {}),
    ).toEqual({});
    expect(a.writes).toEqual([]);
    expect(b.writes).toEqual([]);
    expect(local.sidebar.collapsed).toEqual({ [path]: true });
  });

  it("writes only changed source fields, keeps unrelated keys and subscribes to original keys", () => {
    const local = source("local");
    const remote = source("remote");
    remote.sidebar.projects[0].folders = [
      { id: "folder", name: "Remote folder" },
    ];
    const a = backend(local);
    const b = backend(remote);
    remote.sidebar.collapsed = { [treeKey("folder")]: true, "/other": true };
    b.values.set(
      `codex-project-tree:${scope}`,
      JSON.stringify({ [treeKey("folder")]: true, "/other": true }),
    );
    const model = mergeSidebar([local, remote]);
    const cache = mergedSidebarPreferences(
      model,
      new Map([
        ["local", a.backend],
        ["remote", b.backend],
      ]),
    );
    const key = `codex-project-tree:${model.data.stateDir}`;
    const previous = cache.saved<Record<string, boolean>>(key, {});
    const mapped = model.mapTreeKey("remote", treeKey("folder"));
    let updates = 0;
    const stop = cache.subscribePreference(key, () => updates++);
    cache.editPreference(key, previous, { ...previous, [mapped]: false });
    expect(a.writes).toEqual([]);
    expect(b.writes).toEqual([`codex-project-tree:${scope}`]);
    expect(b.backend.saved(`codex-project-tree:${scope}`, {})).toEqual({
      [treeKey("folder")]: false,
      "/other": true,
    });
    expect(updates).toBe(1);
    stop();
    expect(
      [...b.listeners.values()].every((listeners) => !listeners.size),
    ).toBe(true);
  });

  it("refuses offline preference changes before touching any source", () => {
    const local = source("local");
    const remote = source("remote", [
      {
        id: path,
        path,
        name: "Remote",
        projectAliases: [{ serverId: "local", projectId: path, name: "local" }],
      },
    ]);
    local.online = false;
    const a = backend(local);
    const b = backend(remote);
    const model = mergeSidebar([local, remote]);
    const cache = mergedSidebarPreferences(
      model,
      new Map([
        ["local", a.backend],
        ["remote", b.backend],
      ]),
    );
    const key = `codex-project-compact:${model.data.stateDir}`;
    const previous = cache.saved<Record<string, boolean>>(key, {});
    expect(() =>
      cache.editPreference(key, previous, {
        ...previous,
        [model.projectKey("local", path)]: true,
      }),
    ).toThrow("offline");
    expect(a.writes).toEqual([]);
    expect(b.writes).toEqual([]);
  });

  it("saves collapse choices for teams supplied only by project metadata", () => {
    const local = source("local");
    const remote = source("remote");
    remote.sidebar.projects[0].peerTeams = [
      { id: "legacy-team", name: "Team", members: ["same"] },
    ];
    const a = backend(local);
    const b = backend(remote);
    const model = mergeSidebar([local, remote]);
    const cache = mergedSidebarPreferences(
      model,
      new Map([
        ["local", a.backend],
        ["remote", b.backend],
      ]),
    );
    const key = `codex-project-tree:${model.data.stateDir}`;
    const native = JSON.stringify([path, "team", "legacy-team"]);
    const previous = cache.saved<Record<string, boolean>>(key, {});
    cache.editPreference(key, previous, {
      ...previous,
      [model.mapTreeKey("remote", native)]: true,
    });
    expect(
      b.backend.saved<Record<string, boolean>>(
        `codex-project-tree:${scope}`,
        {},
      )[native],
    ).toBe(true);
    expect(a.writes).toEqual([]);
    expect(b.writes).toEqual([`codex-project-tree:${scope}`]);
  });

  it("saves an explicit mixed reorder on its owner without changing original or unknown IDs", () => {
    const local = source("local");
    const remote = source("remote");
    const original = JSON.stringify([local.sidebar, remote.sidebar]);
    const model = mergeSidebar([local, remote]);
    const group = itemGroup(model.projectKey("local", path));
    const ids = [...model.order[group]].reverse();
    const write = sidebarOrderWrite(model, "local", {
      ...model.order,
      [group]: ids,
    });
    expect(write.expected_revision).toBe(3);
    expect(write.groups[itemGroup(path)]).toEqual(["missing", "same"]);
    expect(
      decodeSidebarRanks(
        write.groups[JSON.stringify(["combined-sidebar-ranks", group])],
      ),
    ).toEqual(ids);
    expect(write.groups.projects).toEqual([path]);
    expect(JSON.stringify([local.sidebar, remote.sidebar])).toBe(original);
    const accepted = {
      ...local,
      sidebar: {
        ...local.sidebar,
        sidebarOrder: { revision: 4, groups: write.groups },
      },
    };
    expect(mergeSidebar([accepted, remote]).order[group]).toEqual(ids);
    expect(() =>
      sidebarOrderWrite(
        mergeSidebar([local, { ...remote, online: false }]),
        "remote",
        model.order,
      ),
    ).toThrow("offline");
  });
});
