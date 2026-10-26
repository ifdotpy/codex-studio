declare global {
  interface Window {
    radioCreateFixture: {
      fixture: Snapshot;
      opened: string | null;
      reopen: () => void;
      failRefresh: boolean;
      ordinary: boolean;
    };
  }
}
const rootElement = document.getElementById("root");
if (!rootElement) throw new Error("Missing radio fixture root");
import { snapshotFixture } from "../../src/snapshotFixture.ts";
import type { Snapshot } from "../../src/types.ts";
import { radioSnapshot } from "./radio-fixture.ts";

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider, Modal } from "@mantine/core";
import "@mantine/core/styles.css";
import SharedChatCreate from "../../src/components/SharedChatCreate.tsx";
import Sidebar from "../../src/components/Sidebar.tsx";
import "../../src/style.css";
import "../../src/appearance.css";
import "../../src/workspace-layout.css";
import { theme } from "../../src/theme.ts";
const accounts: React.ComponentProps<typeof SharedChatCreate>["accounts"] = {
  archivedAccounts: [],
  defaultAccountGroups: [],
  logins: [],
  supportsDelete: false,
  supportsDisconnect: false,
  defaultAccountKey: "codex",
  accounts: [
    {
      id: "codex",
      label: "Personal",
      email: "personal@example.com",
      provider: "codex",
      status: "ready",
      home: "/fixture",
      source: "fixture",
    },
    {
      id: "claude",
      label: "Work",
      email: "work@example.com",
      provider: "claude",
      status: "ready",
      home: "/fixture",
      source: "fixture",
    },
    ...Array.from(
      { length: 5 },
      (
        _,
        i,
      ): NonNullable<
        React.ComponentProps<typeof SharedChatCreate>["accounts"]
      >["accounts"][number] => ({
        id: "extra" + i,
        label: "Account " + (i + 3),
        email: "account" + i + "@example.com",
        provider: i < 3 ? "codex" : "claude",
        status: "ready",
        home: "/fixture",
        source: "fixture",
      }),
    ),
  ],
};
const initial: Snapshot = {
  ...snapshotFixture(),
  stateDir: "create-test",
  threads: [],
  runtime: {
    ...snapshotFixture().runtime,
    projects: [{ id: "p", path: "/p", name: "Project" }],
    rooms: [],
    requests: [],
    peerTeamsVersion: 1,
    peerTeams: [],
  },
};
window.radioCreateFixture = {
  fixture: initial,
  opened: null,
  reopen: () => {},
  failRefresh: false,
  ordinary: false,
};
function Fixture() {
  const [data, setData] = useState(initial);
  const [show, setShow] = useState<{ path?: string | undefined } | null>({
    path: "/p",
  });
  const [opened, open] = useState<string | null>(null);
  window.radioCreateFixture.fixture = data;
  window.radioCreateFixture.opened = opened;
  window.radioCreateFixture.reopen = () => setShow({ path: "/p" });
  const refresh = async () => {
    if (window.radioCreateFixture.failRefresh)
      throw Error("Snapshot unavailable");
    const result = await fetch("/snapshot");
    setData(radioSnapshot(await result.json()));
  };
  return (
    <MantineProvider theme={theme} defaultColorScheme="dark">
      <Sidebar
        data={data}
        opened={opened}
        open={open}
        newChat={() => (window.radioCreateFixture.ordinary = true)}
        newSharedChat={(path) => setShow({ path })}
        addProject={() => {}}
        changeProject={() => {}}
        projectAccount={() => {}}
        creating={false}
        rename={async () => {}}
        remove={() => {}}
        mobile={false}
        onSearch={() => {}}
        close={() => {}}
        refresh={refresh}
        indicators={new Map()}
        markUnread={() => {}}
        markingRead={new Set()}
      />
      <Modal
        opened={!!show}
        title="New shared chat"
        onClose={() => setShow(null)}
        size="md"
      >
        {show && (
          <SharedChatCreate
            data={data}
            accounts={accounts}
            initialPath={show.path}
            refresh={refresh}
            created={(id) => {
              open(id);
              setShow(null);
            }}
          />
        )}
      </Modal>
    </MantineProvider>
  );
}
createRoot(rootElement).render(<Fixture />);
