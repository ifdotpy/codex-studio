import { expect, it, vi } from "vitest";
import {
  localSidebarBackend,
  type SidebarBackend,
} from "../components/sidebar/services";
import { mergeSidebar, type SidebarSource } from "./mergedSidebar";
import { mergedSidebarPreferences } from "./mergedSidebarPreferences";
import { createSidebarVisualSession } from "./sidebarVisualSession";

const scope = "/same/state";
function source(id: string): SidebarSource {
  const path = `/${id}`;
  return {
    id,
    online: id !== "remote",
    sidebar: {
      stateDir: scope,
      threads: [],
      projects: [
        {
          id: path,
          path,
          name: id,
          folders: [{ id: "folder", name: "Saved folder" }],
        },
      ],
      rooms: [],
      peerTeams: [],
      peerTeamsVersion: 1,
      projectOrganizationVersion: 1,
      sidebarOrder: { revision: 1, groups: {} },
      savedOrder: {},
      compact: { [path]: true },
      collapsed: { [JSON.stringify([path, "folder", "folder"])]: true },
      indicators: [],
      unread: [],
    },
  };
}
function setup() {
  const sources = [source("local"), source("remote")];
  const writes = vi.fn();
  const records = new Map<string, Map<string, unknown>>();
  const backends = new Map(
    sources.map((item) => {
      const values = new Map<string, unknown>([
        [`codex-project-compact:${scope}`, item.sidebar.compact],
        [`codex-project-tree:${scope}`, item.sidebar.collapsed],
      ]);
      records.set(item.id, values);
      const backend: SidebarBackend = {
        ...localSidebarBackend,
        ownerId: item.id,
        saved<T>(key: string, fallback: T): T {
          return (values.get(key) as T | undefined) ?? fallback;
        },
        editPreference(key, previous, next) {
          if (!sources.find((source) => source.id === item.id)?.online)
            throw new Error("The sidebar server is offline.");
          writes(item.id, key, previous, next);
          values.set(key, next);
          return next;
        },
        subscribePreference: () => () => {},
      };
      return [item.id, backend] as const;
    }),
  );
  const session = createSidebarVisualSession();
  const wrap = () => {
    const model = mergeSidebar(sources, 1, true);
    const failure = vi.fn();
    return {
      model,
      backend: session(
        model,
        mergedSidebarPreferences(model, backends),
        failure,
      ),
      failure,
    };
  };
  return { sources, writes, records, wrap };
}

it("opens an offline saved folder without writes and restores its saved value after a fresh snapshot", () => {
  const { sources, writes, records, wrap } = setup();
  const { model, backend, failure } = wrap();
  const field = JSON.stringify([
    model.projectKey("remote", "/remote"),
    "folder",
    model.folderKey("remote", "/remote", "folder"),
  ]);
  const key = `codex-project-tree:${model.data.stateDir}`;
  const previous = backend.saved<Record<string, boolean>>(key, {});
  expect(previous[field]).toBe(true);
  expect(
    backend.editPreference(key, previous, { ...previous, [field]: false })[
      field
    ],
  ).toBe(false);
  expect(backend.saved<Record<string, boolean>>(key, {})[field]).toBe(false);
  sources[0].sidebar = { ...sources[0].sidebar };
  expect(wrap().backend.saved<Record<string, boolean>>(key, {})[field]).toBe(
    false,
  );
  sources[1].online = true;
  expect(wrap().backend.saved<Record<string, boolean>>(key, {})[field]).toBe(
    false,
  );
  sources[1].sidebar = { ...sources[1].sidebar };
  expect(wrap().backend.saved<Record<string, boolean>>(key, {})[field]).toBe(
    true,
  );
  expect(records.get("remote")?.get(`codex-project-tree:${scope}`)).toEqual({
    '["/remote","folder","folder"]': true,
  });
  expect(writes).not.toHaveBeenCalled();
  expect(failure).not.toHaveBeenCalled();
});

it("keeps offline Show more in memory and saves only an explicit later online edit", () => {
  const { sources, writes, records, wrap } = setup();
  let { model, backend } = wrap();
  const field = model.projectKey("remote", "/remote");
  const key = `codex-project-compact:${model.data.stateDir}`;
  let previous = backend.saved<Record<string, boolean>>(key, {});
  expect(
    backend.editPreference(key, previous, { ...previous, [field]: false })[
      field
    ],
  ).toBe(false);
  expect(records.get("remote")?.get(`codex-project-compact:${scope}`)).toEqual({
    "/remote": true,
  });
  expect(writes).not.toHaveBeenCalled();
  sources[1].online = true;
  ({ model, backend } = wrap());
  previous = backend.saved<Record<string, boolean>>(key, {});
  backend.editPreference(key, previous, { ...previous, [field]: true });
  expect(backend.saved<Record<string, boolean>>(key, {})[field]).toBe(true);
  expect(writes).not.toHaveBeenCalled();
  backend.editPreference(
    key,
    { ...previous, [field]: true },
    { ...previous, [field]: false },
  );
  expect(writes).toHaveBeenCalledExactlyOnceWith(
    "remote",
    `codex-project-compact:${scope}`,
    { "/remote": true },
    { "/remote": false },
  );
});

it("reports unrelated write failures without a visual override", () => {
  const { model, backend } = setup().wrap();
  const failed = vi.fn();
  const broken = createSidebarVisualSession()(
    model,
    {
      ...backend,
      editPreference: () => {
        throw new Error("Storage full");
      },
    },
    failed,
  );
  const key = `codex-project-compact:${model.data.stateDir}`;
  const previous = broken.saved<Record<string, boolean>>(key, {});
  expect(broken.editPreference(key, previous, {})).toEqual(previous);
  expect(broken.saved(key, {})).toEqual(previous);
  expect(failed).toHaveBeenCalledOnce();
});

it("selection expansion stays in the session across snapshots and never writes on reconnect", () => {
  const { sources, writes, records, wrap } = setup();
  sources[1].sidebar.collapsed = { "/remote": true };
  const { model, backend } = wrap();
  const field = model.projectKey("remote", "/remote");
  const key = `codex-project-tree:${model.data.stateDir}`;
  const previous = backend.saved<Record<string, boolean>>(key, {});
  expect(
    backend.viewPreference!(key, previous, { ...previous, [field]: false })[
      field
    ],
  ).toBe(false);
  sources[1].online = true;
  sources[1].sidebar = { ...sources[1].sidebar };
  expect(wrap().backend.saved<Record<string, boolean>>(key, {})[field]).toBe(
    false,
  );
  expect(sources[1].sidebar.collapsed).toEqual({ "/remote": true });
  expect(records.get("remote")?.get(`codex-project-tree:${scope}`)).toEqual({
    '["/remote","folder","folder"]': true,
  });
  expect(writes).not.toHaveBeenCalled();
});

it("ignores saved project collapse from the home and remote owners", () => {
  const { sources, writes, wrap } = setup();
  sources[1].online = true;
  sources[0].sidebar.collapsed = { "/local": false };
  sources[1].sidebar.projects[0].projectAliases = [
    { serverId: "local", projectId: "/local", name: "local" },
  ];
  sources[1].sidebar.collapsed = { "/remote": false };
  const { model, backend } = wrap();
  const field = model.projectKey("local", "/local");
  const key = `codex-project-tree:${model.data.stateDir}`;
  expect(
    backend.saved<Record<string, boolean>>(key, {})[field],
  ).toBeUndefined();
  expect(model.collapsed[field]).toBeUndefined();
  expect(writes).not.toHaveBeenCalled();
});
