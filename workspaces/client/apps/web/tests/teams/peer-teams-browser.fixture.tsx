import type { Agent, Snapshot } from "../../src/types.ts";
import type { Dispatch, SetStateAction } from "react";
import { fixtureSnapshot } from "../fixtures/snapshot.ts";
import { required } from "./helpers.ts";
declare global {
  interface Window {
    peerTeamsFixture: {
      set: Dispatch<SetStateAction<Snapshot>>;
      data: Snapshot;
      failRefresh: boolean;
    };
  }
}

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Sidebar from "../../src/components/Sidebar.tsx";
import TeamChats from "../../src/components/shell/messages/TeamChats.tsx";
import "../../src/style.css";
import "../../src/workspace-layout.css";
import "../../src/appearance.css";
import { theme } from "../../src/theme.ts";
const a = (
  id: string,
  name: string,
  cwd = "/p",
  extra: Partial<Agent> = {},
): Agent => ({
  id,
  name,
  cwd,
  source: "managed",
  isLead: true,
  created: 1,
  updated: 1,
  ...extra,
});
const initial: Snapshot = {
  ...fixtureSnapshot([]),
  stateDir: "fixture",
  threads: [
    a("a", "Alpha"),
    a("b", "Beta", "/p", { projectFolder: "f" }),
    a("c", "Gamma"),
    a("x", "Other project", "/other"),
    a("w", "Worker", "/p", { isLead: false, rootId: "a" }),
    a("i", "Imported", "/p", {
      // @ts-expect-error source deliberately uses the obsolete imported value to exclude non-managed chats.
      source: "imported",
    }),
    a("d", "Deleted", "/p", { deletedAt: 1 }),
  ],
  runtime: {
    ...fixtureSnapshot([]).runtime,
    peerTeamsVersion: 1,
    peerTeams: [],
    projects: [
      {
        id: "p",
        path: "/p",
        name: "Project",
        peerTeamsRevision: 0,
        folders: [{ id: "f", name: "Folder", parentId: null }],
      },
    ],
    rooms: [],
    requests: [],
    complaints: [],
    agents: [],
  },
};
function Fixture() {
  const [data, setData] = useState(initial);
  const [opened, open] = useState("a");
  window.peerTeamsFixture = { set: setData, data, failRefresh: false };
  const refresh = async () => {
    if (window.peerTeamsFixture.failRefresh) throw Error("Refresh failed");
    const r = await fetch("/snapshot");
    setData(await r.json());
  };
  return (
    <MantineProvider theme={theme} defaultColorScheme="dark">
      <div style={{ width: 320, maxWidth: "100vw" }}>
        <Sidebar
          data={data}
          opened={opened}
          open={open}
          newChat={() => {}}
          addProject={() => {}}
          changeProject={() => {}}
          projectAccount={() => {}}
          creating={false}
          rename={async () => {}}
          remove={() => {}}
          mobile={true}
          onSearch={() => {}}
          close={() => {}}
          refresh={refresh}
          indicators={new Map()}
          markUnread={() => {}}
          markingRead={new Set()}
        />
      </div>
      <TeamChats
        data={data}
        leadId={opened}
        refresh={refresh}
        notify={() => {}}
      />
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<Fixture />);
