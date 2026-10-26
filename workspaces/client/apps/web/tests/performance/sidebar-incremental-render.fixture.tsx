import type { Agent } from "../../src/types.ts";
import { required } from "./performance-fixture.ts";

import React, { useMemo } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import Sidebar from "../../src/components/Sidebar.tsx";
import SidebarRow from "../../src/components/SidebarRow.tsx";
import { useSnapshot } from "../../src/hooks.ts";
import { syncDatabase, persistProjection } from "../../src/sync/client.ts";
import { chatIndicators } from "../../src/components/chat-status/chatStatusModel.ts";
const noop = () => {};
window.sidebarIncrementalFixtureRows = 0;
window.sidebarIncrementalFixtureJsonReads = 0;
window.sidebarIncrementalFixtureRenders = 0;
window.sidebarIncrementalFixtureActions = [];
const row = (collection: string, id: string, value: unknown, seq: number) => ({
  id: "entity:" + collection + ":" + id,
  seq,
  payload: JSON.stringify({ collection, id, value }),
});
const agents: Agent[] = Array.from({ length: 20 }, (_, i) => ({
  id: "agent-" + i,
  name: "Chat " + i,
  kind: "agent",
  source: "managed",
  isLead: true,
  rootId: "agent-" + i,
  cwd: "/fixture",
  status: "completed",
  created: i,
  threadId: "thread-" + i,
  epoch: 0,
  lastCompletedTurn: "turn-" + i,
  lastCompletedTurnStatus: "completed",
  readStateSupported: true,
}));
const { db } = await syncDatabase();
await db.projections.bulkInsert([
  ...agents.map((a, i) => row("agent", a.id, a, i + 1)),
  row("rule", "r", { id: "r", checks: 0 }, 21),
  row(
    "workspace",
    "current",
    {
      stateDir: "/private-fixture",
      projectOrganizationVersion: 1,
      sidebarOrder: { revision: 1, groups: {} },
      peerTeamsVersion: 1,
    },
    22,
  ),
  { id: "state:entities:ready", seq: 1, payload: "ready" },
]);
const oldDocument = await db.projections.findOne("entity:rule:r").exec();
if (!oldDocument) throw new Error("Missing original rule projection");
const oldPayload = oldDocument.toJSON().payload;
window.sidebarIncrementalSameDocumentVersion =
  oldDocument === (await db.projections.findOne("entity:rule:r").exec());
window.sidebarIncrementalDocumentVersionProof = async () => {
  const current = await db.projections.findOne("entity:rule:r").exec();
  return {
    sameVersion: window.sidebarIncrementalSameDocumentVersion,
    newVersion: current !== oldDocument,
    stableNewVersion:
      current === (await db.projections.findOne("entity:rule:r").exec()),
    immutableOldVersion: oldDocument.toJSON().payload === oldPayload,
  };
};
window.sidebarIncrementalWrite = async (
  collection: string,
  id: string,
  value: unknown,
  seq: number,
  deleted = false,
) => {
  await persistProjection(db.projections, {
    ...row(collection, id, value, seq),
    _deleted: deleted,
  });
};
window.sidebarIncrementalReadyAgain = () =>
  persistProjection(db.projections, {
    id: "state:entities:ready",
    seq: 2,
    payload: "ready",
  });
function Harness() {
  const { data } = useSnapshot();
  window.sidebarIncrementalFixtureRenders++;
  const indicators = useMemo(
    () => (data ? chatIndicators(data) : new Map()),
    [data],
  );
  window.sidebarIncrementalFixtureData = data;
  const rule = data?.runtime?.rules?.[0];
  const checks =
    rule && "checks" in rule && typeof rule.checks === "number"
      ? rule.checks
      : 0;
  return data ? (
    <>
      <Sidebar
        data={data}
        opened={null}
        open={(id) =>
          window.sidebarIncrementalFixtureActions.push([id, checks])
        }
        prepareChat={(id) =>
          window.sidebarIncrementalFixtureActions.push(["prepare", id, checks])
        }
        newChat={noop}
        addProject={noop}
        changeProject={noop}
        projectAccount={noop}
        creating={false}
        rename={async () => {}}
        remove={noop}
        mobile={false}
        onSearch={noop}
        close={noop}
        indicators={indicators}
        markUnread={(a) =>
          window.sidebarIncrementalFixtureActions.push(["unread", a.id, checks])
        }
        markingRead={new Set()}
      />
      <SidebarRow
        row={required(agents[0])}
        serverAlias=""
        showServer={false}
        selected={false}
        renaming={false}
        name=""
        organizing={false}
        compact={false}
        markingRead={false}
        bindings={{
          "data-sidebar-id": "key-row",
          onKeyDown: () =>
            window.sidebarIncrementalFixtureActions.push(["key", checks]),
          onDragOver: () =>
            window.sidebarIncrementalFixtureActions.push(["drag", checks]),
        }}
        actions={{
          prepare: noop,
          open: noop,
          pin: noop,
          rename: noop,
          name: noop,
          cancelRename: noop,
          beginRename: noop,
          unread: noop,
          move: noop,
          changeProject: noop,
          archive: noop,
          remove: noop,
        }}
      />
    </>
  ) : null;
}
createRoot(required(document.getElementById("root"))).render(
  <MantineProvider>
    <Harness />
  </MantineProvider>,
);
window.sidebarIncrementalFixtureAgent = required(agents[19]);
window.sidebarIncrementalFixtureReady = true;

declare global {
  interface Window {
    sidebarIncrementalFixtureRows: number;
    sidebarIncrementalFixtureJsonReads: number;
    sidebarIncrementalFixtureRenders: number;
    sidebarIncrementalFixtureActions: (string | number)[][];
    sidebarIncrementalSameDocumentVersion: boolean;
    sidebarIncrementalDocumentVersionProof: () => Promise<{
      sameVersion: boolean;
      newVersion: boolean;
      stableNewVersion: boolean;
      immutableOldVersion: boolean;
    }>;
    sidebarIncrementalWrite: (
      collection: string,
      id: string,
      value: unknown,
      seq: number,
      deleted?: boolean,
    ) => Promise<void>;
    sidebarIncrementalReadyAgain: () => ReturnType<typeof persistProjection>;
    sidebarIncrementalFixtureData: ReturnType<typeof useSnapshot>["data"];
    sidebarIncrementalFixtureAgent: Agent;
    sidebarIncrementalFixtureReady: boolean;
  }
}
