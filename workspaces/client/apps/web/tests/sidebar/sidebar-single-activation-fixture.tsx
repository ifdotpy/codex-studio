import { sidebarFixtureRadio } from "./sidebar-fixture-values.ts";
import type { Snapshot } from "../../src/types.ts";
import type { AgentEntityDto } from "../../src/generated/api.ts";
import type { SidebarSource } from "../../src/servers/mergedSidebar.ts";
declare global {
  interface Window {
    sidebarActivationPrepared: string;
  }
}
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Sidebar from "../../src/components/Sidebar.tsx";
import CombinedServerSidebar from "../../src/servers/CombinedServerSidebar.tsx";
import { mergeSidebar } from "../../src/servers/mergedSidebar.ts";
import { localSidebarBackend } from "../../src/components/sidebar/services.tsx";
import { theme } from "../../src/theme.ts";
import "../../src/style.css";
import "../../src/appearance.css";
const noop = () => {};
const threads: AgentEntityDto[] = Array.from({ length: 30 }, (_, index) => ({
  id: "chat-" + index,
  name: "Chat " + index + " with a long title reaching the actions",
  source: "managed",
  isLead: true,
  cwd: "/project",
  created: 1,
  updated: 1,
}));
const data: Snapshot = {
  stateDir: "sidebar-single-activation",
  token: "fixture-token",
  nodes: [],
  chats: [],
  edges: [],
  threads,
  runtime: {
    projects: [
      { id: "project", name: "Project", path: "/project", folders: [] },
    ],
    agents: [],
    rooms: [],
    requests: [],
    complaints: [],
    peerTeams: [],
    tasks: [],
    monitors: [],
    rules: [],
    events: [],
    work: [],
    studioErrors: [],
    sidebarOrder: { revision: 0, groups: {} },
  },
};
const scenario = new URLSearchParams(location.search).get("groups");
const groupChat = (
  id: string,
  cwd: string,
  archived = false,
): AgentEntityDto => ({
  id,
  name: id,
  cwd,
  archived,
  source: "managed",
  isLead: true,
  status: "completed",
  created: 1,
  updated: 1,
});
const groupData: Snapshot = {
  ...data,
  threads: [
    ...Array.from({ length: 5 }, (_, directory) =>
      Array.from({ length: 2 }, (_, chat) =>
        groupChat(
          "archived-" + directory + "-" + chat,
          "/archive-only-" + directory,
          true,
        ),
      ),
    ).flat(),
    groupChat("active-chat", "/active"),
    groupChat("mixed-active", "/mixed"),
    groupChat("mixed-archived", "/mixed", true),
    groupChat("compact-old-0", "/compact"),
    groupChat("compact-old-1", "/compact"),
    groupChat("registered-archived", "/registered-archive", true),
  ],
  runtime: {
    ...data.runtime,
    projects: [
      { id: "empty", name: "Registered empty", path: "/empty", folders: [] },
      {
        id: "registered-archive",
        name: "Registered archive",
        path: "/registered-archive",
        folders: [],
      },
    ],
    rooms: [
      {
        id: "shared",
        name: "Shared room",
        projectPath: "/shared-only",
        members: [],
        radio: structuredClone(sidebarFixtureRadio),
      },
    ],
  },
};
const source: SidebarSource = {
  id: "local",
  online: true,
  sidebar: {
    stateDir: groupData.stateDir,
    threads: groupData.threads,
    projects: groupData.runtime.projects,
    rooms: groupData.runtime.rooms,
    peerTeams: [],
    sidebarOrder: groupData.runtime.sidebarOrder,
    peerTeamsVersion: undefined,
    projectOrganizationVersion: undefined,
    savedOrder: {},
    compact: {},
    collapsed: {},
    indicators: [],
    unread: [],
  },
};
const model = mergeSidebar([source], 0, true);
const backends = new Map([["local", localSidebarBackend]]);
const online = () => true;
const navigation = {
  opened: null,
  projects: [],
  chats: [],
  ready: true,
  alerts: [],
  error: "",
};
function Fixture() {
  const [opened, setOpened] = useState<string | null>(null);
  const [opens, setOpens] = useState<string[]>([]);
  return (
    <MantineProvider theme={theme} defaultColorScheme="dark">
      {scenario === "combined" ? (
        <CombinedServerSidebar
          model={model}
          backends={backends}
          online={online}
          refreshOwner={async () => navigation}
          executeCommand={async () => {}}
          aliases={{ local: "Fixture" }}
          current="local"
          navigation={{ local: navigation }}
          send={noop}
          applyNavigation={noop}
          mobile={true}
          hidden={false}
          close={noop}
          settings={noop}
          search={noop}
          notify={noop}
          notice=""
          hideOldChatsThreshold={2}
        />
      ) : (
        <Sidebar
          data={scenario ? groupData : data}
          opened={opened}
          open={(id) => {
            setOpened(id);
            setOpens((old) => [...old, id]);
          }}
          prepareChat={(id) => {
            window.sidebarActivationPrepared = id;
          }}
          newChat={noop}
          addProject={noop}
          changeProject={noop}
          projectAccount={noop}
          deleteProject={noop}
          creating={false}
          rename={async () => {}}
          remove={noop}
          mobile={true}
          onSearch={noop}
          close={noop}
          indicators={new Map()}
          markingRead={new Set()}
          markUnread={noop}
          hideOldChatsThreshold={scenario ? 2 : 100}
        />
      )}
      <output aria-label="Opened chats">{JSON.stringify(opens)}</output>
    </MantineProvider>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Fixture root is missing");
createRoot(root).render(<Fixture />);
