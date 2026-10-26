declare global {
  interface Window {
    // fixture stores scroll handlers in tests/performance/scroll-restart.fixture.tsx.
    radioCreateSnapshot: Snapshot;
    // opened is SessionActivityItem[] in tests/chat/session-activity-browser.fixture.tsx.
    radioOpened: string | null;
    reopen: () => void;
    failRefresh: boolean;
    ordinary: boolean;
  }
}
const rootElement = document.getElementById("root")!;
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
type SharedAccounts = NonNullable<
  React.ComponentProps<typeof SharedChatCreate>["accounts"]
>;
type RadioAccount = Pick<
  SharedAccounts["accounts"][number],
  "id" | "label" | "email" | "provider" | "status"
>;
// Base accounts omit archivedAccounts, defaultAccountGroups, logins, supportsDelete,
// supportsDisconnect, and each account's home/source.
function partialRadioAccounts(
  value: Pick<SharedAccounts, "defaultAccountKey"> & {
    accounts: RadioAccount[];
  },
): SharedAccounts {
  return value as SharedAccounts;
}
const accounts = partialRadioAccounts({
  defaultAccountKey: "codex",
  accounts: [
    {
      id: "codex",
      label: "Personal",
      email: "personal@example.com",
      provider: "codex",
      status: "ready",
    },
    {
      id: "claude",
      label: "Work",
      email: "work@example.com",
      provider: "claude",
      status: "ready",
    },
    ...Array.from({ length: 5 }, (_, i): RadioAccount => ({
      id: "extra" + i,
      label: "Account " + (i + 3),
      email: "account" + i + "@example.com",
      provider: i < 3 ? "codex" : "claude",
      status: "ready",
    })),
  ],
});
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
window.radioCreateSnapshot = initial;
window.radioOpened = null;
window.reopen = () => {};
window.failRefresh = false;
window.ordinary = false;
function Fixture() {
  const [data, setData] = useState(initial);
  const [show, setShow] = useState<{ path?: string | undefined } | null>({
    path: "/p",
  });
  const [opened, open] = useState<string | null>(null);
  window.radioCreateSnapshot = data;
  window.radioOpened = opened;
  window.reopen = () => setShow({ path: "/p" });
  const refresh = async () => {
    if (window.failRefresh) throw Error("Snapshot unavailable");
    const result = await fetch("/snapshot");
    setData(radioSnapshot(await result.json()));
  };
  return (
    <MantineProvider theme={theme} defaultColorScheme="dark">
      <Sidebar
        data={data}
        opened={opened}
        open={open}
        newChat={() => (window.ordinary = true)}
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
