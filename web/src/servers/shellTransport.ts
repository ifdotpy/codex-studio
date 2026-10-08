import type { DesktopBridge } from "../desktop";
import type { CredentialRequest, StreamChunk } from "./desktopCredentials";
import { serverParentOrigin, serverViewId } from "./environment";
import { readServers, localServer } from "./registry";
import { serverCredentialAdapter } from "./transport";
// The shell derives an owner from the live window and its exact origin. A view
// can request only its own transport. No key enters a server view.
export function bindShellTransport(frames: Map<string, HTMLIFrameElement>) {
  const active = new Map<
    string,
    { controller: AbortController; frame: HTMLIFrameElement }
  >();
  const listener = async (event: MessageEvent) => {
    if (event.data?.kind !== "studio-server-transport") return;
    const frameEntry = [...frames].find(
      ([, frame]) =>
        frame.contentWindow === event.source &&
        frame.isConnected &&
        new URL(frame.src).origin === event.origin,
    );
    if (!frameEntry) return;
    const [owner, frame] = frameEntry;
    const { correlation, value } = event.data as {
      correlation: string;
      value: CredentialRequest;
    };
    const reply = (result: unknown, error?: string) =>
      frame.contentWindow?.postMessage(
        { kind: "studio-server-transport-result", correlation, result, error },
        event.origin,
      );
    let ownedKey: string | undefined;
    try {
      if (
        typeof correlation !== "string" ||
        !value ||
        value.serverId !== owner ||
        (value.frameOwner && value.frameOwner !== owner)
      )
        throw new Error("Invalid server frame owner.");
      const key = owner + ":" + value.streamId;
      if (value.action === "cancel") {
        active.get(key)?.controller.abort();
        return reply(undefined);
      }
      if (value.action !== "request" || !value.streamId || active.has(key))
        throw new Error("Invalid server transport action.");
      const server =
        owner === "local"
          ? localServer()
          : readServers().find((row) => row.id === owner);
      if (
        !server ||
        new URL(value.url!).origin !== server.origin ||
        !new URL(value.url!).pathname.startsWith("/api/") ||
        !["GET", "POST", "HEAD"].includes(value.method!)
      )
        throw new Error("The request does not belong to this server.");
      const controller = new AbortController();
      active.set(key, { controller, frame });
      ownedKey = key;
      const request = new Request(value.url!, {
        method: value.method,
        headers: value.headers,
        ...(value.body?.byteLength ? { body: value.body } : {}),
        signal: controller.signal,
      });
      const response =
        owner === "local"
          ? await fetch(request)
          : await serverCredentialAdapter().fetch(server, request);
      const metadata = {
        status: response.status,
        statusText: response.statusText,
        headers: [...response.headers],
      };
      if (
        response.body &&
        response.headers.get("Content-Type")?.startsWith("text/event-stream")
      ) {
        reply({ ...metadata, streamId: value.streamId });
        const reader = response.body.getReader();
        const send = (chunk: Omit<StreamChunk, "serverId" | "streamId">) => {
          if (!frame.isConnected || frames.get(owner) !== frame) {
            controller.abort();
            return;
          }
          frame.contentWindow?.postMessage(
            {
              kind: "studio-server-stream",
              value: { serverId: owner, streamId: value.streamId, ...chunk },
            },
            event.origin,
          );
        };
        try {
          while (!controller.signal.aborted) {
            const chunk = await reader.read();
            if (chunk.done) break;
            send({
              bytes: chunk.value.buffer.slice(
                chunk.value.byteOffset,
                chunk.value.byteOffset + chunk.value.byteLength,
              ) as ArrayBuffer,
            });
          }
          send({ done: true });
        } catch {
          send({ error: "The server connection closed." });
        } finally {
          controller.abort();
          await reader.cancel().catch(() => {});
          active.delete(key);
        }
      } else {
        const body = await response.arrayBuffer();
        active.delete(key);
        reply({ ...metadata, body });
      }
    } catch (error) {
      if (ownedKey) {
        active.get(ownedKey)?.controller.abort();
        active.delete(ownedKey);
      }
      reply(undefined, (error as Error).message);
    }
  };
  const removed = new MutationObserver(() => {
    for (const [key, stream] of active)
      if (!stream.frame.isConnected) {
        stream.controller.abort();
        active.delete(key);
      }
  });
  removed.observe(document.body, { childList: true, subtree: true });
  window.addEventListener("message", listener);
  return () => {
    window.removeEventListener("message", listener);
    removed.disconnect();
    for (const stream of active.values()) stream.controller.abort();
    active.clear();
  };
}
export function shellCredentialBridge(): DesktopBridge {
  const pending = new Map<
    string,
    { resolve: (value: unknown) => void; reject: (error: Error) => void }
  >();
  const streams = new Set<(value: StreamChunk) => void>();
  window.addEventListener("message", (event) => {
    if (event.source !== window.parent || event.origin !== serverParentOrigin)
      return;
    if (
      event.data?.kind === "studio-server-stream" &&
      event.data.value?.serverId === serverViewId
    )
      for (const listener of streams) listener(event.data.value);
    if (event.data?.kind === "studio-server-transport-result") {
      const handler = pending.get(event.data.correlation);
      if (!handler) return;
      pending.delete(event.data.correlation);
      if (event.data.error) handler.reject(new Error(event.data.error));
      else handler.resolve(event.data.result);
    }
  });
  return {
    serverCredentialAction: (value) =>
      new Promise((resolve, reject) => {
        const correlation = crypto.randomUUID();
        pending.set(correlation, { resolve, reject });
        window.parent.postMessage(
          { kind: "studio-server-transport", correlation, value },
          serverParentOrigin,
        );
      }),
    onServerStream: (callback) => {
      streams.add(callback);
      return () => {
        streams.delete(callback);
      };
    },
  } as DesktopBridge;
}
