import { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import AccountSignIn from "../../src/components/AccountSignIn.tsx";
import Accounts, {
  type AccountsState,
} from "../../src/components/Accounts.tsx";
import Workspace from "../../src/components/shell/Workspace.tsx";
import type { Agent, Snapshot } from "../../src/types.ts";
import type { JsonInput } from "../../src/apiTypes.ts";
import { accountErrorFailure } from "./account-errors-data.ts";
import {
  fixtureAccount,
  fixtureAccounts,
  fixtureRuntime,
  required,
} from "./fixture-helpers.ts";
const { negative }: { negative: boolean } = await (
  await fetch("/fixture-data")
).json();
const agent = {
  id: "lead",
  rootId: "lead",
  isLead: true,
  source: "managed",
  status: "idle",
  model: "gpt-6",
  name: "Fixture lead",
  created: 1,
  cwd: "/fixture",
} satisfies Agent;
localStorage.setItem("account-sign-in:fixture", JSON.stringify("request"));
declare global {
  interface Window {
    accountErrorsFixture: {
      changeError(error: JsonInput): void;
      showSurface(surface: string): void;
    };
  }
}
function Harness() {
  const [error, setError] = useState<JsonInput>(accountErrorFailure);
  const [surface, setSurface] = useState("signin");
  window.accountErrorsFixture = {
    changeError: setError,
    showSurface: setSurface,
  };
  const data: AccountsState = fixtureAccounts({
    accounts: [
      fixtureAccount({
        id: "default",
        label: "Fixture account",
        status: "ready",
        // @ts-expect-error Deliberately structured account error verifies diagnostic rendering beyond the string error wire contract.
        error,
      }),
    ],
    defaultAccountKey: "default",
    logins: [
      {
        requestId: "request",
        accountKey: "default",
        status: "error",
        // @ts-expect-error Negative control deliberately renders a structured receipt error instead of the wire string.
        error: negative ? accountErrorFailure : accountErrorFailure.message,
      },
    ],
  });
  const state = {
    scope: "fixture",
    data,
    setData: () => {},
    refresh: async () => null,
    error: "",
  };
  const snapshot: Snapshot = {
    token: "fixture",
    stateDir: "fixture",
    threads: [agent],
    runtime: { ...fixtureRuntime({ agents: [agent] }), studioErrors: [] },
    nodes: [agent],
    edges: [],
    chats: [],
  };
  return (
    <MantineProvider>
      <p data-shell>Saved conversation remains available</p>
      {surface === "signin" ? (
        <AccountSignIn state={state} />
      ) : surface === "accounts" ? (
        <Accounts state={state} managerOnly onError={() => {}} />
      ) : (
        <Workspace
          key={surface}
          opened
          initialSection={surface}
          agent={agent}
          data={snapshot}
          allRequests={[]}
          onClose={() => window.accountErrorsFixture.showSurface("signin")}
          onSelect={() => {}}
          refresh={async () => {}}
          notify={() => {}}
        />
      )}
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<Harness />);
