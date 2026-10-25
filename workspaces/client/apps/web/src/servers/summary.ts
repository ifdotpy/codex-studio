import { createServerTransport } from "./serverTransport";
import { hasNativeClient } from "../native/client";
import { nativeFetch } from "../native/http";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../generated/apiSchema";
import type { StudioServer } from "./registry";
import { serverCredentialAdapter } from "./transport";
import type { CredentialResponse } from "./desktopCredentials";
import type { ServerNavigation } from "./navigation";
import { readServerAccounts, type ServerAccount } from "./navigation";
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
/** The server runs another Studio version: it answers, but its API differs from this one. */
export class SchemaMismatchError extends Error {
  constructor() {
    super("The server summary requires a Studio update.");
  }
}

/** The connection state that a failed summary request shows. */
export const summaryFailure = (error: unknown) =>
  error instanceof SchemaMismatchError
    ? ("schema-mismatch" as const)
    : ("offline" as const);

export async function fetchServerSummary(
  server: StudioServer,
  signal: AbortSignal,
): Promise<ServerNavigation & { busy: boolean; accounts: ServerAccount[] }> {
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
        ? hasNativeClient()
          ? await nativeFetch(request, server.origin)
          : await fetch(request)
        : await createServerTransport(server, (owner, request) =>
            serverCredentialAdapter().fetch(owner, request),
          ).fetch(request);
  if (!response.ok) {
    await response.body?.cancel().catch(() => {});
    throw new Error("The server summary is unavailable.");
  }
  if (response.headers.get(API_SCHEMA_HASH_HEADER) !== API_SCHEMA_HASH) {
    await response.body?.cancel().catch(() => {});
    throw new SchemaMismatchError();
  }
  const value = await response.json();
  if (
    !value ||
    !Array.isArray(value.chats) ||
    !Array.isArray(value.projects) ||
    !Array.isArray(value.alerts) ||
    !Array.isArray(value.accounts) ||
    typeof value.ready !== "boolean" ||
    typeof value.busy !== "boolean" ||
    typeof value.system !== "string" ||
    typeof value.agentsRunning !== "number"
  )
    throw new Error("Invalid server summary.");
  const accounts = readServerAccounts(value.accounts);
  if (!accounts) throw new Error("Invalid server account summary.");
  return { ...value, accounts, opened: null, error: "" };
}
