import type { Agent } from "../../src/types.ts";
import type { Account } from "../../src/components/Accounts.tsx";
import { fixtureAccount } from "./fixture-helpers.ts";
declare global {
  interface Window {
    claudeSettingsAuthFixture: { signInAccount: string };
  }
}
const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { ClaudeSettings } from "../../src/components/ClaudeSettings.tsx";
import ClaudeSignIn from "../../src/components/ClaudeSignIn.tsx";
const agent: Agent = {
  id: "chat",
  accountKey: "claude-work",
  provider: "claude",
  status: "idle",
  model: "sonnet",
};
function Fixture() {
  const [account, setAccount] = useState<Account>(
    fixtureAccount({
      id: "claude-work",
      label: "Work",
      email: "work@example.com",
      provider: "claude",
      status: "signedOut",
    }),
  );
  const [login, setLogin] = useState(false);
  return (
    <MantineProvider>
      <ClaudeSettings
        agent={agent}
        account={account}
        onSignIn={(key) => {
          window.claudeSettingsAuthFixture = { signInAccount: key };
          setLogin(true);
        }}
      />
      {login && (
        <ClaudeSignIn
          account={account}
          scope="settings-fixture"
          onClose={() => setLogin(false)}
          onReady={() => {
            const ready = structuredClone(account);
            ready.status = "ready";
            setAccount(ready);
            setLogin(false);
          }}
        />
      )}
    </MantineProvider>
  );
}
createRoot(fixtureRoot).render(<Fixture />);
