import type {
  ServerCredentialAdapter,
  PairAttemptIdentity,
  AutomaticPairApproval,
} from "../servers/transport";
import type { StudioServer } from "../servers/registry";
import {
  parseInvitation,
  requestIdentity,
  PairingNotAppliedError,
} from "../servers/pairing";
import { nativeRequest } from "./client";
function encode(bytes: Uint8Array) {
  const parts: string[] = [];
  for (let index = 0; index < bytes.length; index += 16384)
    parts.push(String.fromCharCode(...bytes.subarray(index, index + 16384)));
  return btoa(parts.join(""));
}
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
  async fetch(server: StudioServer, request: Request): Promise<Response> {
    request.signal.throwIfAborted();
    const bytes = new Uint8Array(await request.clone().arrayBuffer());
    if (bytes.length > 32 * 1024 * 1024)
      throw new Error("The server request is too large.");
    const response = await nativeRequest<{
      status: number;
      headers: [string, string][];
      stream?: string;
    }>("servers.request", {
      serverId: server.id,
      credentialId: server.credentialId,
      url: request.url,
      method: request.method,
      headers: [...request.headers],
      base64: encode(bytes),
      requestId: await requestIdentity(request),
    });
    const cancel = () =>
      response.stream
        ? nativeRequest("servers.cancel", { stream: response.stream }).then(
            () => undefined,
          )
        : Promise.resolve();
    if (request.signal.aborted) {
      await cancel();
      throw request.signal.reason;
    }
    if (!response.stream) return new Response(null, response);
    let stopped = false;
    let controller: ReadableStreamDefaultController<Uint8Array>;
    const cleanup = () => {
      stopped = true;
      request.signal.removeEventListener("abort", abort);
    };
    const abort = () => {
      if (stopped) return;
      cleanup();
      controller.error(request.signal.reason);
      void cancel().catch(() => {});
    };
    const body = new ReadableStream<Uint8Array>({
      start(value) {
        controller = value;
        request.signal.addEventListener("abort", abort, { once: true });
        if (request.signal.aborted) abort();
      },
      async pull() {
        if (stopped) return;
        try {
          const chunk = await nativeRequest<{
            done?: boolean;
            base64?: string;
          }>("servers.read", { stream: response.stream });
          if (stopped) return;
          if (chunk.done) {
            cleanup();
            controller.close();
          } else
            controller.enqueue(
              Uint8Array.from(atob(chunk.base64!), (char) =>
                char.charCodeAt(0),
              ),
            );
        } catch (error) {
          if (!stopped) {
            cleanup();
            controller.error(error);
            void cancel().catch(() => {});
          }
        }
      },
      cancel() {
        cleanup();
        return cancel();
      },
    });
    return new Response(body, response);
  }
}
