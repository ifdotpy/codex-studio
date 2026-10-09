import { get, post, refreshSession } from "../api";
import { serverLocalStorage } from "./storage";
import type { ResourceConnectionState } from "../sync/resourceEvents";
import type { StudioServer } from "./registry";

export type ProjectLocation = {
  serverId: string;
  path: string;
  projectId: string;
  requestedPath?: string | null;
  gitOrigin?: string | null;
  gitHead?: string | null;
};
export type LocationProject = {
  id: string;
  path: string;
  name: string;
  homeServerId?: string | null;
  compact?: boolean;
  locations?: ProjectLocation[] | null;
  projectAliases?:
    | { serverId: string; projectId: string; name: string }[]
    | null;
};
export type ProjectServerChoice = {
  id: string;
  label: string;
  disabled?: boolean;
  serverId?: string;
};
export function projectServerChoices(
  servers: StudioServer[],
  statuses: Record<string, ResourceConnectionState>,
  reachability: Record<string, string | null | undefined> = {},
): ProjectServerChoice[] {
  return servers.map((server) => ({
    id: server.id,
    label: server.id === "local" ? "This Mac" : server.label,
    disabled:
      reachability[server.id] === "unreachable" ||
      ["offline", "degraded", "schema-mismatch"].includes(
        statuses[server.id] || "",
      ),
  }));
}
export async function readProjectServers(): Promise<ProjectServerChoice[]> {
  const snapshot = await get("/api/multi-server");
  return [
    { id: "local", serverId: snapshot.identity.serverId, label: "This Mac" },
    ...snapshot.servers
      .filter(
        (server) =>
          server.status !== "discovered" && server.status !== "revoked",
      )
      .map((server) => ({
        id: server.id,
        label: server.label,
        disabled:
          server.status === "unreachable" ||
          server.reachability === "unreachable",
      })),
  ];
}
export function durableProjectRequest(
  body: unknown,
  proposed: string,
  storage: Pick<
    Storage,
    "getItem" | "setItem" | "removeItem"
  > = serverLocalStorage,
) {
  const key = "studio-project-request:" + JSON.stringify(body);
  const existing = storage.getItem(key);
  const id = existing || proposed;
  storage.setItem(key, id);
  const finish = () => {
    if (storage.getItem(key) === id) storage.removeItem(key);
  };
  return {
    id,
    finish,
    // An HTTP error cannot prove whether the destination applied the effect.
    reject(_error: unknown) {
      return id;
    },
  };
}
export class ProjectReceiptRefused extends Error {}

export async function saveProjectLocation(body: {
  action: "add_location" | "remove_location";
  project: string;
  server: string;
  path?: string;
  request_id: string;
}) {
  const session = await refreshSession();
  const { request_id: proposed, ...content } = body;
  const request = durableProjectRequest(content, proposed);
  const result = await post(
    "/api/projects",
    { ...content, request_id: request.id },
    { sessionToken: session.token },
  ).catch((error: unknown) => {
    request.reject(error);
    throw error;
  });
  if ("outcome" in result && result.outcome === "not_applied") {
    request.finish();
    throw new ProjectReceiptRefused(
      result.error || "The location change was refused.",
    );
  }
  if (!("outcome" in result) || result.outcome !== "applied")
    throw new Error(
      "The location change is pending. Retry this request to read its result.",
    );
  request.finish();
  return result;
}
export async function registerProject(
  path: string,
  server: string,
  requestId: string,
) {
  const session = await refreshSession();
  const content = { path, ...(server !== "local" ? { server } : {}) };
  const request = durableProjectRequest(content, requestId);
  const result = await post(
    "/api/projects",
    {
      ...content,
      ...(server !== "local" ? { request_id: request.id } : {}),
    },
    { sessionToken: session.token },
  ).catch((error: unknown) => {
    request.reject(error);
    throw error;
  });
  if ("outcome" in result && result.outcome === "not_applied") {
    request.finish();
    throw new ProjectReceiptRefused(
      result.error || "The project request was refused.",
    );
  }
  if ("outcome" in result && result.outcome !== "applied")
    throw new Error(
      "The project request is pending. Retry this request to read its result.",
    );
  request.finish();
  return result;
}

export function projectLocationOn(
  project: LocationProject,
  server: ProjectServerChoice,
) {
  return (
    project.locations?.find(
      (location) => location.serverId === (server.serverId || server.id),
    ) ||
    (server.id === "local"
      ? project.locations?.find(
          (location) => location.serverId === project.homeServerId,
        )
      : undefined)
  );
}
