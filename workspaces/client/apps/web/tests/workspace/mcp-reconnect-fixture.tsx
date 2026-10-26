import type { ServerServices } from "../../src/servers/serverServices.ts";
import { ServerServicesProvider } from "../../src/servers/ServerServicesContext.tsx";
import type {
  ApiGetPath,
  ApiPostPath,
  GetResult,
  PostBody,
  PostResult,
} from "../../src/apiTypes.ts";
import type { GetOptions } from "../../src/api.ts";
import type { Agent, Snapshot } from "../../src/types.ts";
import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { Workspace } from "../../src/components/shell/Workspace.tsx";
const agent: Agent = {
  id: "agent",
  name: "Claude fixture",
  source: "managed",
  provider: "claude",
  isLead: true,
};
async function get<Path extends ApiGetPath>(
  path: Path,
  options?: GetOptions<Path>,
): Promise<GetResult<Path>> {
  const response = await fetch(
    path + "?" + new URLSearchParams(options?.query as Record<string, string>),
  );
  return response.json();
}
async function post<Path extends ApiPostPath>(
  path: Path,
  body: PostBody<Path>,
): Promise<PostResult<Path>> {
  const response = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const result = await response.json();
  if (!response.ok) throw new Error(result.error);
  return result;
}
// This one-off service fixture omits API methods, storage, and synchronization services unused by Tools.
const services: Partial<Record<keyof ServerServices, unknown>> = {
  owner: { id: "fixture", origin: location.origin },
  api: { get, post },
  watchResourceReads: () => () => {},
};
// This one-off snapshot omits token, nodes, edges, chats, and unused runtime collections.
const data: Partial<Record<keyof Snapshot, unknown>> = {
  stateDir: "/fixture",
  threads: [agent],
  runtime: {
    agents: [agent],
    requests: [],
    complaints: [],
    messages: [],
    rooms: [],
    monitors: [],
    tasks: [],
    work: [],
    rules: [],
  },
};
createRoot(document.getElementById("root")!).render(
  <MantineProvider>
    <ServerServicesProvider services={services as ServerServices}>
      <Workspace
        initialSection="tools"
        opened={true}
        onClose={() => {}}
        agent={agent}
        data={data as Snapshot}
        allRequests={[]}
        onSelect={() => {}}
        refresh={async () => {}}
        notify={() => {}}
      />
    </ServerServicesProvider>
  </MantineProvider>,
);
