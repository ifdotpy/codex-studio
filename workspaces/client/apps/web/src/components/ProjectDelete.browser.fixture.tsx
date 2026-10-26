import type { Agent, Snapshot } from "../types.ts";
import type { SidebarBackend, SidebarServices } from "./sidebar/services.tsx";
import type { PostBody, PostResult } from "../api.ts";
import {
  fixturePostBody,
  fixturePostResult,
} from "../../tests/misc-fixture.ts";
declare global {
  interface Window {
    posts: { path: string; body: PostBody<"/api/projects/delete"> }[];
    finished: string[][];
    suspended: string[];
    fail: boolean;
    retire: () => void;
    notice: string | undefined;
    owner: string | undefined;
  }
}
const rootElement = document.getElementById("root");

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import Sidebar from "../components/Sidebar.tsx";
import ProjectGroupsSidebar from "../servers/ProjectGroupsSidebar.tsx";
import { navigationSnapshot } from "../servers/navigation.ts";
import { ProjectDeleteDialog } from "../components/ProjectDeleteDialog.tsx";
import { useProjectDelete } from "../useProjectDelete.ts";
import "@mantine/core/styles.css";
import "../design/layout.css";
import "../appearance.css";
import "../components/sidebar-projects.css";
const project = {
  id: "project",
  name: "Example",
  path: "/project",
  homeServerId: "home",
  projectSeq: 12,
};
const agent: Agent = {
  id: "lead",
  cwd: "/project",
  source: "managed",
  isLead: true,
  name: "Chat",
  status: "paused",
  projectId: "project",
  projectServerId: "home",
};
// This one-off sidebar fixture omits the server token and unrelated runtime fields.
type ProjectDeleteFixtureSnapshot = Partial<Omit<Snapshot, "runtime">> & {
  runtime: Partial<Snapshot["runtime"]>;
};
const partialInitial: ProjectDeleteFixtureSnapshot = {
  stateDir: "fixture",
  entityWorkspaceId: "workspace",
  entitiesComplete: true,
  threads: [agent],
  chats: [],
  nodes: [agent],
  edges: [],
  runtime: {
    projectSequences: { project: 12 },
    agents: [agent],
    projects: [project],
    rooms: [],
    requests: [],
    tasks: [],
    monitors: [],
    complaints: [],
    rules: [],
    peerTeams: [],
    events: [],
    work: [],
    projectOrganizationVersion: 1,
    peerTeamsVersion: 1,
  },
};
const initial = partialInitial as Snapshot;

window.posts = [];
window.finished = [];
window.suspended = [];
window.fail = false;
window.retire = () => {};
const store = new Map<string, string>();
const backend: SidebarBackend = {
  ownerId: "owner",
  storage: {
    getItem: (key) => store.get(key) || null,
    setItem: (key, value) => store.set(key, value),
    removeItem: (key) => store.delete(key),
  },
  saved: <T,>(key: string, value: T): T => {
    // This fixture writes and reads its own saved-state values.
    return (JSON.parse(store.get(key) || "null") as T | null) ?? value;
  },
  save: () => {},
  editPreference: (_key, _before, after) => after,
  subscribePreference: () => () => {},
  post: async (path, body) => {
    const request = fixturePostBody<"/api/projects/delete">(body);
    window.posts.push({ path, body: request });
    if (window.fail) throw new TypeError("response lost");
    const response = {
      id: request.id,
      request_id: request.request_id,
      outcome: "deleted",
      deletedAgentIds: ["lead"],
      deletedRoomIds: [],
    } satisfies PostResult<"/api/projects/delete">;
    return fixturePostResult<typeof path>(response);
  },
};
const services: SidebarServices = {
  cache: backend,
  owner: () => backend,
  byOwner: () => backend,
  serverForChat: () => "owner",
};
function Fixture() {
  const [data, setData] = useState(
    location.search.includes("empty")
      ? {
          ...initial,
          threads: [],
          nodes: [],
          runtime: { ...initial.runtime, agents: [] },
        }
      : initial,
  );
  const [, setScope] = useState<ReadonlySet<string>>(new Set());
  window.retire = () =>
    setData({
      ...initial,
      threads: [],
      nodes: [],
      runtime: { ...initial.runtime, agents: [], projects: [] },
    });
  const deletion = useProjectDelete({
    data,
    workspaceId: "workspace",
    services,
    onSuspend: (scope) => {
      window.suspended = [...scope];
      setScope(scope);
    },
    onResume: () => setScope(new Set()),
    onFinish: (ids) => {
      window.finished.push(ids);
      setScope(new Set());
    },
    notify: (message) => {
      window.notice = message;
    },
  });
  const logical = location.search.includes("logical");
  return React.createElement(
    React.Fragment,
    null,
    logical
      ? React.createElement(ProjectGroupsSidebar, {
          navigation: {
            owner: {
              ...navigationSnapshot(data, "lead", ""),
              projects: data.runtime.projects.map((project) => ({
                ...project,
                ...(location.search.includes("legacy") ? { id: "" } : {}),
              })) as React.ComponentProps<
                typeof ProjectGroupsSidebar
              >["navigation"][string]["projects"],
              // The source fixture intentionally omits the unrelated unread field.
              chats: data.threads.map((chat) => ({
                ...chat,
                path: chat.cwd,
                archived: false,
              })) as React.ComponentProps<
                typeof ProjectGroupsSidebar
              >["navigation"][string]["chats"],
              ready: true,
              opened: "lead",
              alerts: [],
              error: "",
            },
          },
          servers: [],
          statuses: {},
          current: "owner",
          query: "",
          hideOldChatsThreshold: 20,
          send: (owner, command) => {
            window.owner = owner;
            if (command.action === "delete-project")
              deletion.open(command.projectId);
          },
        })
      : React.createElement(Sidebar, {
          services,
          data,
          opened: "lead",
          indicators: new Map(),
          markingRead: new Set<string>(),
          creating: false,
          mobile: true,
          settings: () => {},
          onSearch: () => {},
          close: () => {},
          open: () => {},
          newChat: () => {},
          addProject: () => {},
          changeProject: () => {},
          projectAccount: () => {},
          rename: async () => {},
          remove: () => {},
          markUnread: () => {},
          deleteProject: (path: string) => deletion.open(path),
          refresh: async () => {
            throw new Error("Unexpected refresh");
          },
        }),
    React.createElement(ProjectDeleteDialog, deletion.dialog),
  );
}
createRoot(rootElement!).render(
  React.createElement(
    React.StrictMode,
    null,
    React.createElement(MantineProvider, null, React.createElement(Fixture)),
  ),
);
