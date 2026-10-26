import { fixtureSnapshot } from "../fixtures/snapshot.ts";
import type { Agent, Complaint, Request, Snapshot } from "../../src/types.ts";
declare global {
  interface Window {
    dialogDraft_notices: string[];
    dialogDraft_restoreStorage: () => void;
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
const request = {
  id: "question",
  agent: "lead",
  method: "item/tool/requestUserInput",
  params: {
    questions: [
      { id: "public", question: "Public answer" },
      { id: "secret", question: "Private answer", isSecret: true },
    ],
  },
} satisfies Request;
const base: Snapshot = fixtureSnapshot([lead]);
base.stateDir = "workspace-a";
base.runtime.complaints = [complaint];
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
        notify={(message) => window.dialogDraft_notices.push(message)}
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
window.dialogDraft_notices = [];
createRoot(
  document.getElementById("root") ??
    (() => {
      throw new Error("Missing mount");
    })(),
).render(<Fixture />);
