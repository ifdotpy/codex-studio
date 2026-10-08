import { serverViewId, isolatedServerView } from "./environment";
import { viewServer, type StudioServer } from "./registry";

// Pairing owns credentials and signing. This boundary never puts a credential
// in a URL, snapshot, project row, or navigation message.
export type PairAttemptIdentity = {
  requestId: string;
  serverId: string;
  origin: string;
  inviteId?: string;
};
export interface ServerCredentialAdapter {
  hasPairAttempt(attempt: PairAttemptIdentity): Promise<boolean>;
  pair(origin: string, code: string, requestId: string): Promise<StudioServer>;
  fetch(server: StudioServer, request: Request): Promise<Response>;
  forget(server: StudioServer): Promise<void>;
}
let credentials: ServerCredentialAdapter | undefined;
export function setServerCredentialAdapter(adapter: ServerCredentialAdapter) {
  credentials = adapter;
}
export function serverCredentialAdapter() {
  if (!credentials)
    throw new Error("Server pairing is unavailable in this client.");
  return credentials;
}
export function apiOrigin() {
  return serverViewId
    ? viewServer(serverViewId).origin
    : (globalThis.location?.origin ?? "http://localhost");
}
export function serverFetch(request: Request) {
  if (!serverViewId || (serverViewId === "local" && !isolatedServerView))
    return globalThis.fetch(request);
  const server = viewServer(serverViewId);
  if (new URL(request.url).origin !== server.origin)
    throw new Error("The request does not belong to this server.");
  return serverCredentialAdapter().fetch(server, request);
}
