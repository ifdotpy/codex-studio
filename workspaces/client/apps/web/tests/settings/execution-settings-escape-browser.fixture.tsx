import type { Agent } from "../../src/types.ts";
import { fixtureAccount } from "./fixture-helpers.ts";
const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider, Modal, Button } from "@mantine/core";
import "@mantine/core/styles.css";
import { ExecutionSettings } from "../../src/components/agents/ExecutionSettings.tsx";
const agent: Agent = {
  id: "escape",
  accountKey: "first",
  provider: "codex",
  name: "Lead",
  source: "managed",
  model: "gpt-6-astra",
  effort: "high",
  isLead: true,
  status: "idle",
  created: 1,
};
const models = [
  {
    model: "gpt-6-astra",
    supportedReasoningEfforts: [
      { reasoningEffort: "low" },
      { reasoningEffort: "high" },
    ],
    serviceTiers: [],
  },
];
function Fixture() {
  const [parent, setParent] = useState(true),
    [opened, setOpened] = useState(false),
    [account, setAccount] = useState(false);

  return (
    <MantineProvider>
      <Modal
        opened={parent}
        title="Chat settings"
        onClose={() => setParent(false)}
        closeOnEscape={!opened && !account}
      >
        <ExecutionSettings
          agent={agent}
          catalog={{ models, loading: false, error: "", retry: () => {} }}
          accounts={[
            fixtureAccount({ id: "first", label: "First", status: "ready" }),
            fixtureAccount({ id: "second", label: "Second", status: "ready" }),
          ]}
          onAccountChange={() => setAccount(true)}
          onOpenChange={setOpened}
          accountModalOpen={account}
          refresh={async () => {}}
        />
      </Modal>
      <Modal
        opened={account}
        onClose={() => setAccount(false)}
        title="Change account"
        zIndex={350}
      >
        <Button>Confirm account</Button>
      </Modal>
    </MantineProvider>
  );
}
createRoot(fixtureRoot).render(<Fixture />);
