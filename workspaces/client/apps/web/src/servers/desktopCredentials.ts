import type {
  ServerCredentialAdapter,
  PairAttemptIdentity,
  AutomaticPairApproval,
} from "./transport";
import type { StudioServer } from "./registry";
import {
  parseInvitation,
  requestIdentity,
  PairingNotAppliedError,
} from "./pairing";
import { serverViewId } from "./environment";
export type {
  CredentialRequest,
  CredentialResponse,
  StreamChunk,
} from "../../../desktop/api.ts";
import type { CredentialBridge, StreamChunk } from "../../../desktop/api.ts";
import { isServerMethod } from "../../../desktop/api.ts";
export class DesktopServerCredentials implements ServerCredentialAdapter {
  declare private bridge: CredentialBridge;
  constructor(bridge: CredentialBridge) {
    this.bridge = bridge;
  }
  async hasPairAttempt(attempt: PairAttemptIdentity): Promise<boolean> {
    const result = await this.bridge.serverCredentialAction({
      action: "pairAttempt",
      ...attempt,
    });
    if (typeof result !== "boolean")
      throw new Error("The credential state response is invalid.");
    return result;
  }
  async pair(
    origin: string,
    code: string,
    requestId: string,
    approval?: AutomaticPairApproval,
  ): Promise<StudioServer> {
    const result = await this.bridge.serverCredentialAction({
      ...(approval ? { action: "pairApproved", approval } : { action: "pair" }),
      origin,
      invitation: parseInvitation(code, origin),
      requestId,
    });
    if ("notApplied" in result) throw new PairingNotAppliedError();
    return result;
  }
  async fetch(server: StudioServer, request: Request): Promise<Response> {
    const method = request.method;
    if (!isServerMethod(method)) throw new Error("Unsupported server method.");
    const streamId = crypto.randomUUID();
    let receiver: ReadableStreamDefaultController<Uint8Array> | undefined;
    let ready = false;
    const buffered: StreamChunk[] = [];
    const apply = (value: StreamChunk) => {
      if (!receiver) return;
      if (value.error) {
        request.signal.removeEventListener("abort", abort);
        receiver.error(new TypeError(value.error));
        stop();
      } else if (value.done) {
        request.signal.removeEventListener("abort", abort);
        receiver.close();
        stop();
      } else if (value.bytes) receiver.enqueue(new Uint8Array(value.bytes));
    };
    const stop = this.bridge.onServerStream((value) => {
      if (value.serverId !== server.id || value.streamId !== streamId) return;
      if (!ready) buffered.push(value);
      else apply(value);
    });
    const cancel = () => {
      stop();
      void this.bridge
        .serverCredentialAction({
          action: "cancel",
          serverId: server.id,
          streamId,
          frameOwner: serverViewId || undefined,
        })
        .catch(() => {});
    };
    const abort = () => {
      cancel();
      receiver?.error(
        request.signal.reason || new DOMException("Aborted", "AbortError"),
      );
    };
    if (request.signal.aborted) {
      stop();
      throw request.signal.reason;
    }
    request.signal.addEventListener("abort", abort, { once: true });
    try {
      const result = await this.bridge.serverCredentialAction({
        action: "request",
        serverId: server.id,
        credentialId: server.credentialId,
        url: request.url,
        method,
        headers: [...request.headers],
        body: await request.clone().arrayBuffer(),
        requestId: await requestIdentity(request),
        streamId,
        frameOwner: serverViewId || undefined,
      });
      if (request.signal.aborted) {
        cancel();
        throw request.signal.reason;
      }
      if (!result.streamId) {
        stop();
        request.signal.removeEventListener("abort", abort);
        return new Response(
          result.status === 204 || result.status === 304 ? null : result.body,
          {
            status: result.status,
            statusText: result.statusText,
            headers: result.headers,
          },
        );
      }
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          receiver = controller;
          ready = true;
          buffered.forEach(apply);
        },
        cancel() {
          request.signal.removeEventListener("abort", abort);
          cancel();
        },
      });
      return new Response(body, {
        status: result.status,
        statusText: result.statusText,
        headers: result.headers,
      });
    } catch (error) {
      cancel();
      request.signal.removeEventListener("abort", abort);
      throw error;
    }
  }
  async forget(server: StudioServer) {
    await this.bridge.serverCredentialAction({
      action: "forget",
      serverId: server.id,
    });
  }
}
export function nativeCredentialBridge(): CredentialBridge | undefined {
  const bridge = window.codexDesktop;
  if (bridge?.serverCredentialAction && bridge.onServerStream)
    return {
      serverCredentialAction: bridge.serverCredentialAction,
      onServerStream: bridge.onServerStream,
    };
  return undefined;
}
