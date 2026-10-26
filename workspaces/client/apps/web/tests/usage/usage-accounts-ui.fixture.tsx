import type { Agent } from "../../src/types.ts";
import {
  fixtureRoot,
  usageSessionCost,
  usageAccountCost,
} from "./usage-fixture.ts";

import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import type { UsageAccount } from "../../src/components/Usage.tsx";
import { createServerServices } from "../../src/servers/serverServices.ts";
import { snapshotFixture } from "../../src/snapshotFixture.ts";
import Usage from "../../src/components/Usage.tsx";
import { ServerServicesProvider } from "../../src/servers/ServerServicesContext.tsx";
import * as sync from "../../src/sync/client.ts";
const baseServices = createServerServices({
  id: "local",
  label: "Fixture",
  origin: location.origin,
});
const services = {
  ...baseServices,
  owner: { id: "fixture", label: "Fixture", origin: location.origin },
  sync: {
    ...sync,
    subscribeStateProjection(
      accept: Parameters<typeof sync.subscribeStateProjection>[0],
    ) {
      accept(
        snapshotFixture([], {
          sessionCostsByAgent: {
            chat: usageSessionCost(),
          },
          accountCostsByAccount: {
            own: usageAccountCost("own"),
          },
        }),
      );
      return () => {};
    },
  },
};
import { accountLimitFixtures } from "../../src/components/AccountLimits.fixtures.ts";
import { theme } from "../../src/theme.ts";
const now = Number(new URLSearchParams(location.search).get("now"));
const geometry = location.search.includes("geometry");
if (geometry) {
  await import("../../src/style.css");
  await import("../../src/design/state.css");
}
const own: Agent = {
  id: "chat",
  rootId: "chat",
  accountKey: "own",
  source: "managed",
  model: "gpt-6-astra",
  status: "completed",
  isLead: true,
  created: 1,
  contextUsage: { tokens: 10, window: 100 },
};
const ownLimits = {
  accountKey: "own",
  at: now,
  data: {
    accountId: "own-account",
    rateLimits: {
      limitId: "codex",
      primary: { usedPercent: 20, resetsAt: now + 3600 },
    },
  },
};
const teamLimits = {
  accountKey: "team",
  at: now,
  data: {
    accountId: "team-account",
    rateLimits: {
      limitId: "codex",
      primary: { usedPercent: 92, resetsAt: now + 3600 },
    },
    rateLimitResetCredits: {
      availableCount: 1,
      credits: [
        {
          id: "team-credit",
          title: "Team reset",
          status: "available",
          resetType: "codexRateLimits",
        },
      ],
    },
  },
};
declare global {
  interface Window {
    usageAccountsFixture: {
      heartbeats: number;
      reloadCalls: [string, boolean][];
    };
  }
}
window.usageAccountsFixture = { heartbeats: 0, reloadCalls: [] };
const NativeEventSource = window.EventSource;
window.EventSource = class extends NativeEventSource {
  constructor(...args: ConstructorParameters<typeof NativeEventSource>) {
    super(...args);
    this.addEventListener(
      "heartbeat",
      () => window.usageAccountsFixture.heartbeats++,
    );
  }
};
window.usageAccountsFixture.reloadCalls = [];
const scheme = new URLSearchParams(location.search).get("scheme");
if (scheme !== null && scheme !== "light" && scheme !== "dark")
  throw new Error("Invalid fixture color scheme");
function Fixture() {
  const [multi] = useState(location.search.includes("multi"));
  const [teamReady, setTeamReady] = useState(false);
  const accounts: UsageAccount[] = geometry
    ? accountLimitFixtures
    : [
        {
          key: "own",
          label: "Own account",
          email: "own@example.test",
          accountId: "own-account",
          limits: ownLimits,
          loading: false,
          reload: (force = false) => {
            window.usageAccountsFixture.reloadCalls.push(["own", force]);
          },
        },
        ...(multi
          ? ([
              {
                key: "team",
                label: "Team account",
                email: "team@example.test",
                provider: "openai",
                accountId: "team-account",
                limits: teamReady ? teamLimits : null,
                loading: false,
                reload: (force = false) => {
                  window.usageAccountsFixture.reloadCalls.push(["team", force]);
                  if (!teamReady) setTeamReady(true);
                },
              },
              {
                key: "claude",
                label: "Claude work",
                email: "claude@example.test",
                provider: "claude",
                limits: {
                  accountKey: "claude",
                  at: now,
                  data: {
                    rateLimits: {
                      limitId: "claude",
                      primary: { usedPercent: 50, resetsAt: now + 3600 },
                    },
                  },
                },
                loading: false,
                reload: (force = false) => {
                  window.usageAccountsFixture.reloadCalls.push([
                    "claude",
                    force,
                  ]);
                },
              },
              {
                key: "unnamed",
                label: "unnamed",
                provider: "codex",
                limits: {
                  accountKey: "unnamed",
                  at: now,
                  data: {
                    rateLimits: {
                      limitId: "codex",
                      primary: { usedPercent: 85, resetsAt: now + 3600 },
                    },
                  },
                },
                loading: false,
                reload: (force = false) => {
                  window.usageAccountsFixture.reloadCalls.push([
                    "unnamed",
                    force,
                  ]);
                },
              },
            ] satisfies UsageAccount[])
          : []),
      ];
  return (
    <MantineProvider
      theme={theme}
      {...(scheme === "light" || scheme === "dark"
        ? { forceColorScheme: scheme }
        : {})}
    >
      <div
        style={
          geometry
            ? {
                display: "flex",
                alignItems: "end",
                width: "100%",
                height: "100dvh",
                padding: "var(--mantine-spacing-sm)",
                containerType: "inline-size",
                containerName: "conversation-footer",
              }
            : undefined
        }
      >
        <Usage
          agent={geometry ? { ...own, accountKey: "codex-main" } : own}
          stateDir="/fixture"
          limits={ownLimits}
          accountLabel="own@example.test"
          reload={() => {
            window.usageAccountsFixture.reloadCalls.push(["fallback", true]);
          }}
          opened={true}
          onChange={() => {}}
          limitsLoading={false}
          accounts={accounts}
        />
      </div>
    </MantineProvider>
  );
}
createRoot(fixtureRoot()).render(
  <ServerServicesProvider services={services}>
    <Fixture />
  </ServerServicesProvider>,
);
