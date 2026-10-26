import { createServerServices } from "../../src/servers/serverServices.ts";
import { ServerServicesProvider } from "../../src/servers/ServerServicesContext.tsx";
import { get, post } from "./fixture-api.ts";
import type { Agent } from "../../src/types.ts";
import { required } from "../teams/helpers.ts";
import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { Workspace } from "../../src/components/shell/Workspace.tsx";
import { fixtureSnapshot } from "../fixtures/snapshot.ts";
const agent: Agent = {
  id: "agent",
  name: "Claude fixture",
  source: "managed",
  provider: "claude",
  isLead: true,
};
const defaults = createServerServices({
  id: "fixture",
  label: "Fixture",
  origin: location.origin,
});
const services = {
  ...defaults,
  api: { ...defaults.api, get, post },
  watchResourceReads: () => Object.assign(() => {}, { refresh: () => {} }),
};
const data = fixtureSnapshot([agent]);
createRoot(required(document.getElementById("root"))).render(
  <MantineProvider>
    <ServerServicesProvider services={services}>
      <Workspace
        initialSection="tools"
        opened={true}
        onClose={() => {}}
        agent={agent}
        data={data}
        allRequests={[]}
        onSelect={() => {}}
        refresh={async () => {}}
        notify={() => {}}
      />
    </ServerServicesProvider>
  </MantineProvider>,
);
