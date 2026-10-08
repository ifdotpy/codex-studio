import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../generated/apiSchema";
import type { StudioServer } from "./registry";
import { serverCredentialAdapter } from "./transport";
import type { CredentialResponse } from "./desktopCredentials";
import type { ServerNavigation } from "./navigation";
export function isSummaryRequest(request: Request, server: StudioServer) {
  const url = new URL(request.url);
  return (
    request.method === "GET" &&
    url.origin === server.origin &&
    url.pathname === "/api/ui-summary" &&
    !url.search &&
    !url.hash &&
    !request.body
  );
}
export async function fetchServerSummary(
  server: StudioServer,
  signal: AbortSignal,
): Promise<ServerNavigation & { busy: boolean }> {
  const request = new Request(server.origin + "/api/ui-summary", {
    headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH },
    signal,
    redirect: "error",
  });
  if (!isSummaryRequest(request, server))
    throw new Error("Invalid shell summary request.");
  let response: Response;
  if (window.codexDesktop) {
    const value = (await window.codexDesktop.serverCredentialAction!({
      action: "summary",
      serverId: server.id,
      credentialId: server.credentialId,
      url: request.url,
      method: "GET",
      headers: [...request.headers],
    })) as CredentialResponse;
    response = new Response(value.body, {
      status: value.status,
      headers: value.headers,
    });
  } else
    response =
      server.id === "local"
        ? await fetch(request)
        : await serverCredentialAdapter().fetch(server, request);
  if (!response.ok) throw new Error("The server summary is unavailable.");
  if (response.headers.get(API_SCHEMA_HASH_HEADER) !== API_SCHEMA_HASH)
    throw new Error("The server summary requires a Studio update.");
  const value = await response.json();
  if (
    !value ||
    !Array.isArray(value.chats) ||
    !Array.isArray(value.projects) ||
    !Array.isArray(value.alerts) ||
    typeof value.ready !== "boolean" ||
    typeof value.busy !== "boolean" ||
    typeof value.system !== "string" ||
    typeof value.agentsRunning !== "number"
  )
    throw new Error("Invalid server summary.");
  return { ...value, opened: null, error: "" };
}
