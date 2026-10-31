import type { Agent } from "../../src/types.ts";

import React, { useMemo } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import Sidebar from "../../src/components/Sidebar.tsx";
import SidebarRow from "../../src/components/SidebarRow.tsx";
import { useSnapshot } from "../../src/hooks.ts";
import { syncDatabase, persistProjection } from "../../src/sync/client.ts";
import { chatIndicators } from "../../src/components/chat-status/chatStatusModel.ts";
const noop = () => {};
window.fixtureRows = 0;
window.fixtureJsonReads = 0;
window.fixtureRenders = 0;
window.fixtureActions = [];
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
await db.projections.hydrate([
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
const oldPayload = oldDocument!.toJSON().payload;
window.sameDocumentVersion =
  oldDocument === (await db.projections.findOne("entity:rule:r").exec());
window.documentVersionProof = async () => {
  const current = await db.projections.findOne("entity:rule:r").exec();
  return {
    sameVersion: window.sameDocumentVersion,
    newVersion: current !== oldDocument,
    stableNewVersion:
      current === (await db.projections.findOne("entity:rule:r").exec()),
    immutableOldVersion: oldDocument!.toJSON().payload === oldPayload,
  };
};
window.write = async (
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
window.readyAgain = () =>
  persistProjection(db.projections, {
    id: "state:entities:ready",
    seq: 2,
    payload: "ready",
  });
// This one-off row fixture omits serverAlias and showServer, as on the base.
const partialSidebarRowProps = {} as Pick<
  React.ComponentProps<typeof SidebarRow>,
  "serverAlias" | "showServer"
>;
function Harness() {
  const { data } = useSnapshot();
  window.fixtureRenders++;
  const indicators = useMemo(
    () => (data ? chatIndicators(data) : new Map()),
    [data],
  );
  window.fixtureData = data;
  const checks =
    (
      data?.runtime?.rules?.[0] as
        | (NonNullable<typeof data>["runtime"]["rules"][number] & {
            checks: number;
          })
        | undefined
    )?.checks || 0;
  return data ? (
    <>
      <Sidebar
        data={data}
        opened={null}
        open={(id) => window.fixtureActions.push([id, checks])}
        prepareChat={(id) =>
          window.fixtureActions.push(["prepare", id, checks])
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
        markUnread={(a) => window.fixtureActions.push(["unread", a.id, checks])}
        markingRead={new Set()}
      />
      <SidebarRow
        row={agents[0]!}
        {...partialSidebarRowProps}
        selected={false}
        renaming={false}
        name=""
        organizing={false}
        compact={false}
        markingRead={false}
        bindings={{
          "data-sidebar-id": "key-row",
          onKeyDown: () => window.fixtureActions.push(["key", checks]),
          onDragOver: () => window.fixtureActions.push(["drag", checks]),
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
createRoot(document.getElementById("root")!).render(
  <MantineProvider>
    <Harness />
  </MantineProvider>,
);
window.fixtureAgent = agents[19]!;
window.fixtureReady = true;

declare global {
  interface Window {
    fixtureRows: number;
    fixtureJsonReads: number;
    fixtureRenders: number;
    fixtureActions: (string | number)[][];
    sameDocumentVersion: boolean;
    documentVersionProof: () => Promise<{
      sameVersion: boolean;
      newVersion: boolean;
      stableNewVersion: boolean;
      immutableOldVersion: boolean;
    }>;
    write: (
      collection: string,
      id: string,
      value: unknown,
      seq: number,
      deleted?: boolean,
    ) => Promise<void>;
    readyAgain: () => ReturnType<typeof persistProjection>;
    fixtureData: ReturnType<typeof useSnapshot>["data"];
    fixtureAgent: Agent;
    fixtureReady: boolean;
  }
}
