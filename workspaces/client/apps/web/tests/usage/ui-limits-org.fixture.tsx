import type { Agent } from "../../src/types.ts";
import type { Account } from "../../src/components/Accounts.tsx";
import { fixtureRoot } from "./usage-fixture.ts";

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider, Button } from "@mantine/core";
import "@mantine/core/styles.css";
import Usage from "../../src/components/Usage.tsx";
import { NativeError } from "../../src/components/NativeNotice.tsx";
import Accounts from "../../src/components/Accounts.tsx";
import ClaudeProfile from "../../src/components/ClaudeProfile.tsx";
import AccountSignInNotice from "../../src/components/AccountSignInNotice.tsx";
import "../../src/components/accounts.css";
import "../../src/style.css";
import "../../src/appearance.css";
import { theme } from "../../src/theme.ts";
declare global {
  interface Window {
    limitsOrgFixture: { refresh: () => void; signedInAccount?: string };
  }
}
window.limitsOrgFixture = { refresh: () => {} };
const expected = {
  name: "Research organization for the selected Claude profile",
  idSuffix: "12345678",
};
const observed = {
  name: "Another organization with a separate subscription",
  idSuffix: "87654321",
};
const account: Account = {
  id: "claude",
  home: "",
  source: "fixture",
  label: "Research",
  email: "research@example.test",
  provider: "claude",
  status: "changed",
  organization: expected,
  organizationMismatch: { expected, observed },
};
const now = Date.now() / 1000;
const agent: Agent = {
  id: "worker",
  rootId: "worker",
  epoch: 1,
  isLead: true,
  status: "failed",
  accountKey: "claude",
  provider: "claude",
  model: "sonnet",
  threadId: "thread",
  nativeLimitErrorAt: now - 20,
  error: {
    codexErrorInfo: "rateLimitExceeded",
    message: "Usage limit reached",
  },
};
function Fixture() {
  const [screen, setScreen] = useState("limits");
  const [at, setAt] = useState(now);
  const [limitsOpen, setLimitsOpen] = useState(false);
  window.limitsOrgFixture.refresh = () => setAt(Date.now() / 1000);
  const limits = {
    accountKey: "claude",
    at,
    eligibility: {
      agentId: "another-worker",
      accountKey: "claude",
      provider: "claude",
      model: "opus",
      allowed: true,
      canRun: true,
      fresh: true,
      checkedAt: now,
      validUntil: now + 3600,
      resetAt: null,
      buckets: [],
      overage: "unavailable",
    },
    data: {
      rateLimits: {
        limitId: "claude",
        primary: { usedPercent: 100, resetsAt: now + 3600 },
      },
      rateLimitsByLimitId: {
        "claude-opus": {
          limitName: "Opus",
          secondary: { usedPercent: 100, resetsAt: now + 7200 },
        },
        "claude-sonnet": {
          limitName: "Sonnet",
          secondary: { usedPercent: 20, resetsAt: now + 3600 },
        },
      },
    },
  };
  return (
    <MantineProvider theme={theme} defaultColorScheme="auto">
      <main style={{ padding: 12, maxWidth: 900, margin: "auto", minWidth: 0 }}>
        <Button
          onClick={() => setScreen(screen === "limits" ? "accounts" : "limits")}
        >
          {screen === "limits" ? "Show accounts" : "Show limits"}
        </Button>
        {screen === "limits" ? (
          <>
            <div data-notice>
              <NativeError
                agent={agent}
                limits={limits}
                refreshLimits={() => window.limitsOrgFixture.refresh()}
              />
            </div>
            <div
              style={{ position: "fixed", bottom: 12, right: 12 }}
              data-usage
            >
              <Usage
                agent={agent}
                stateDir="fixture"
                limits={limits}
                accounts={[
                  {
                    key: "claude",
                    provider: "claude",
                    label: "Research",
                    limits,
                    loading: false,
                    reload: async () => window.limitsOrgFixture.refresh(),
                  },
                ]}
                accountLabel="Research"
                opened={limitsOpen}
                onChange={setLimitsOpen}
                reload={async () => window.limitsOrgFixture.refresh()}
              />
            </div>
          </>
        ) : (
          <>
            <AccountSignInNotice
              account={account}
              errors={[]}
              onSignIn={(key) =>
                (window.limitsOrgFixture.signedInAccount = key)
              }
            />
            <Accounts
              managerOnly
              state={{
                data: {
                  accounts: [account],
                  archivedAccounts: [],
                  defaultAccountKey: "claude",
                  defaultAccountGroups: [],
                  logins: [],
                  supportsDelete: false,
                  supportsDisconnect: false,
                },
                setData: () => {},
                refresh: async () => null,
                error: "",
                scope: "fixture",
              }}
              onError={() => {}}
            />
            <ClaudeProfile account={account} onSaved={() => {}} expanded />
          </>
        )}
      </main>
    </MantineProvider>
  );
}
createRoot(fixtureRoot()).render(<Fixture />);
