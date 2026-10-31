import type { Agent, Complaint, Snapshot } from "../../src/types.ts";
import { approval } from "../../src/approvalFixtures.ts";
declare global {
  interface Window {
    notices: string[];
    restoreStorage: () => void;
  }
}

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Picker from "../../src/components/ProjectDirectoryPicker.tsx";
import UserMessages from "../../src/components/shell/messages/UserMessages.tsx";
import MessageDate from "../../src/components/conversation/transcript/MessageDate.tsx";
import Requests from "../../src/components/questions/Requests.tsx";
const lead = {
  id: "lead",
  rootId: "lead",
  name: "Lead",
  isLead: true,
  source: "managed",
} satisfies Agent;
const complaint = {
  id: "complaint",
  leadId: "lead",
  author: "lead",
  authorName: "Lead",
  recipient: "user",
  title: "Help needed",
  status: "open",
  needsUserResponse: true,
  created: 1,
} satisfies Complaint;
const request = approval({
  id: "question",
  agent: "lead",
  request: {
    kind: "question",
    questions: [
      {
        id: "public",
        question: "Public answer",
        header: null,
        options: [],
        is_secret: false,
        multi_select: null,
        is_other: null,
      },
      {
        id: "secret",
        question: "Private answer",
        header: null,
        options: [],
        is_secret: true,
        multi_select: null,
        is_other: null,
      },
    ],
  },
});
// This partial snapshot omits unused collections and runtime fields.
const base = {
  stateDir: "workspace-a",
  token: "fixture",
  threads: [lead],
  runtime: { complaints: [complaint] },
} as Snapshot;
function Fixture() {
  const [show, setShow] = useState(false),
    [workspace, setWorkspace] = useState("workspace-a");
  return (
    <MantineProvider>
      <button onClick={() => setShow(!show)}>Toggle messages</button>
      <button
        onClick={() =>
          setWorkspace(
            workspace === "workspace-a" ? "workspace-b" : "workspace-a",
          )
        }
      >
        Switch workspace
      </button>
      <div data-missing-date>
        <MessageDate at={0} />
      </div>
      <Requests
        requests={[request]}
        allRequests={[request]}
        scope={workspace}
        agents={[lead]}
        refresh={async () => {}}
        notify={(message) => window.notices.push(message)}
      />
      <Picker initialPath="/project" onSelect={async () => {}} />
      {show && (
        <UserMessages
          data={{ ...base, stateDir: workspace }}
          refresh={async () => {}}
          notify={() => {}}
        />
      )}
    </MantineProvider>
  );
}
window.notices = [];
createRoot(document.getElementById("root")!).render(<Fixture />);
