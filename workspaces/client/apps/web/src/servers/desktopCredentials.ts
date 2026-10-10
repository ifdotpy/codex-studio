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
import type { DesktopBridge } from "../desktop";
export type CredentialRequest = {
  action:
    | "pair"
    | "pairApproved"
    | "request"
    | "summary"
    | "cancel"
    | "forget"
    | "pairAttempt";
  approval?: AutomaticPairApproval;
  serverId?: string;
  credentialId?: string;
  origin?: string;
  invitation?: unknown;
  inviteId?: string;
  requestId?: string;
  url?: string;
  method?: string;
  headers?: [string, string][];
  body?: ArrayBuffer;
  streamId?: string;
  frameOwner?: string;
};
export type CredentialResponse = {
  status: number;
  statusText: string;
  headers: [string, string][];
  body?: ArrayBuffer;
  streamId?: string;
};
export type StreamChunk = {
  serverId: string;
  streamId: string;
  bytes?: ArrayBuffer;
  done?: boolean;
  error?: string;
};
export class DesktopServerCredentials implements ServerCredentialAdapter {
  constructor(private bridge: DesktopBridge) {}
  async hasPairAttempt(attempt: PairAttemptIdentity): Promise<boolean> {
    const result = await this.bridge.serverCredentialAction!({
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
    const result = await this.bridge.serverCredentialAction!({
      action: approval ? "pairApproved" : "pair",
      ...(approval ? { approval } : {}),
      origin,
      invitation: parseInvitation(code, origin),
      requestId,
    });
    if (
      result &&
      typeof result === "object" &&
      "notApplied" in result &&
      result.notApplied === "invite_unavailable"
    )
      throw new PairingNotAppliedError();
    return result as StudioServer;
  }
  async fetch(server: StudioServer, request: Request): Promise<Response> {
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
    const stop = this.bridge.onServerStream!((value) => {
      if (value.serverId !== server.id || value.streamId !== streamId) return;
      if (!ready) buffered.push(value);
      else apply(value);
    });
    const cancel = () => {
      stop();
      void this.bridge.serverCredentialAction!({
        action: "cancel",
        serverId: server.id,
        streamId,
        frameOwner: serverViewId || undefined,
      }).catch(() => {});
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
      const result = (await this.bridge.serverCredentialAction!({
        action: "request",
        serverId: server.id,
        credentialId: server.credentialId,
        url: request.url,
        method: request.method,
        headers: [...request.headers],
        body: await request.clone().arrayBuffer(),
        requestId: await requestIdentity(request),
        streamId,
        frameOwner: serverViewId || undefined,
      })) as CredentialResponse;
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
    await this.bridge.serverCredentialAction!({
      action: "forget",
      serverId: server.id,
    });
  }
}
export function nativeCredentialBridge(): DesktopBridge | undefined {
  if (window.codexDesktop?.serverCredentialAction) return window.codexDesktop;
  return undefined;
}
