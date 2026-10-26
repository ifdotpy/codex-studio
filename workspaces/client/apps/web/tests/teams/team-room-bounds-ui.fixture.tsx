import type { Agent, Room, Snapshot } from "../../src/types.ts";
import { fixtureSnapshot } from "../fixtures/snapshot.ts";
import { required } from "./helpers.ts";
declare global {
  interface Window {
    showRoomBounds: () => void;
  }
}

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import TeamChats from "../../src/components/shell/messages/TeamChats.tsx";
import "../../src/style.css";
import "../../src/workspace-layout.css";
import "../../src/appearance.css";
import { theme } from "../../src/theme.ts";
const lead: Agent = {
  id: "lead",
  name: "Lead",
  rootId: "lead",
  isLead: true,
  source: "managed",
  created: 1,
  updated: 1,
};
const worker: Agent = {
  id: "worker",
  name: "Worker",
  rootId: "lead",
  isLead: false,
  source: "managed",
  created: 1,
  updated: 1,
};
const room: Room = {
  id: "private:lead:worker",
  kind: "private",
  members: ["lead", "worker"],
  name: "Worker",
  updated: 1,
  lastMessage: {
    seq: 1391,
    text: "room message 1391",
    created: 1,
    sender: "worker",
  },
};
const data: Snapshot = {
  ...fixtureSnapshot([]),
  stateDir: "room-fixture",
  threads: [lead, worker],
  runtime: {
    ...fixtureSnapshot([]).runtime,
    rooms: [room],
    requests: [],
    complaints: [],
    projects: [],
    peerTeams: [],
  },
};
const app = createRoot(required(document.getElementById("root")));
window.showRoomBounds = () =>
  app.render(
    <MantineProvider theme={theme} defaultColorScheme="dark">
      <TeamChats
        data={data}
        leadId="lead"
        refresh={async () => {}}
        notify={() => {}}
      />
    </MantineProvider>,
  );
window.showRoomBounds();
