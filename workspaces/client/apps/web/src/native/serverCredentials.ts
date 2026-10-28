import type {
  ServerCredentialAdapter,
  PairAttemptIdentity,
  AutomaticPairApproval,
} from "../servers/transport";
import type { StudioServer } from "../servers/registry";
import { parseInvitation, PairingNotAppliedError } from "../servers/pairing";
import { coreForServer, nativeRequest } from "./client";
import { nativeFetch } from "./http";

/**
 * Pairing keys stay in the desktop or, for a browser page, on the server of
 * the page. The page receives only the server row, never a key.
 */
export class NativeServerCredentials implements ServerCredentialAdapter {
  hasPairAttempt(attempt: PairAttemptIdentity) {
    return nativeRequest<boolean>("servers.pairAttempt", attempt);
  }
  async pair(
    origin: string,
    code: string,
    requestId: string,
    approval?: AutomaticPairApproval,
  ): Promise<StudioServer> {
    const result = await nativeRequest<StudioServer | { notApplied: string }>(
      "servers.pair",
      {
        origin,
        invitation: parseInvitation(code, origin),
        requestId,
        approval,
      },
      requestId,
    );
    if ("notApplied" in result) throw new PairingNotAppliedError();
    return result;
  }
  async forget(server: StudioServer) {
    await nativeRequest("servers.forget", { serverId: server.id });
  }
  /** A paired server's requests go through its own client core, which signs them. */
  fetch(server: StudioServer, request: Request): Promise<Response> {
    return nativeFetch(request, server.origin, coreForServer(server.id));
  }
}
