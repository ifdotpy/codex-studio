import { sidebarAgent } from "./sidebarSnapshot";
import type { Agent } from "../types";
import { ApiError, post, type PostBody, type PostOptions } from "../api";
import {
  isServerCommand,
  type ServerCommand,
  type ServerNavigation,
} from "./navigation";

const sidebarPaths = [
  "/api/projects",
  "/api/organization",
  "/api/peer-teams",
  "/api/rename",
] as const;
export type SidebarPostPath = (typeof sidebarPaths)[number];
type PostRequest = {
  action: "post";
  path: SidebarPostPath;
  body: PostBody<SidebarPostPath>;
  options?: Pick<PostOptions, "timeoutMs" | "requestId">;
};
export type SidebarRpcRequest =
  | PostRequest
  | { action: "refresh" }
  | {
      action: "command";
      command: Extract<
        ServerCommand,
        { action: "prepare-chat" | "mark-unread" }
      >;
    };
type Pending = {
  frame: HTMLIFrameElement;
  owner: string;
  origin: string;
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
};

/** The reply must come from the exact frame used for this request. */
export function createSidebarRpcClient(
  surface: Window,
  frames: Map<string, HTMLIFrameElement>,
  ready: (owner: string) => Promise<void>,
) {
  const pending = new Map<string, Pending>();
  let listening = false;
  const receive = (event: MessageEvent) => {
    if (event.data?.kind !== "studio-sidebar-result") return;
    const request = pending.get(event.data.correlation);
    if (
      !request ||
      event.source !== request.frame.contentWindow ||
      event.origin !== request.origin ||
      event.data.serverId !== request.owner
    )
      return;
    pending.delete(event.data.correlation);
    clearTimeout(request.timer);
    if (event.data.error) {
      const { message, status, payload } = event.data.error;
      request.reject(
        typeof status === "number"
          ? new ApiError(message, status, payload)
          : new Error(message),
      );
    } else request.resolve(event.data.result);
  };
  const start = () => {
    if (!listening) {
      surface.addEventListener("message", receive);
      listening = true;
    }
  };
  return {
    start,
    async request(owner: string, value: SidebarRpcRequest): Promise<unknown> {
      start();
      await ready(owner);
      const frame = frames.get(owner);
      if (!frame?.isConnected || !frame.contentWindow)
        throw new Error("The sidebar server is unavailable.");
      const origin = new URL(frame.src).origin;
      const correlation = crypto.randomUUID();
      return new Promise((resolve, reject) => {
        const timer = setTimeout(
          () => {
            pending.delete(correlation);
            reject(
              new Error(
                "The sidebar request result is unknown. Retry the saved request.",
              ),
            );
          },
          value.action === "post"
            ? (value.options?.timeoutMs ?? 15000) + 2000
            : 20000,
        );
        pending.set(correlation, {
          frame,
          owner,
          origin,
          resolve,
          reject,
          timer,
        });
        frame.contentWindow!.postMessage(
          {
            kind: "studio-sidebar-request",
            serverId: owner,
            correlation,
            value,
          },
          origin,
        );
      });
    },
    dispose() {
      surface.removeEventListener("message", receive);
      listening = false;
      for (const request of pending.values()) {
        clearTimeout(request.timer);
        request.reject(new Error("The sidebar request result is unknown."));
      }
      pending.clear();
    },
  };
}

/** Keep token, mutation identity and sync coverage inside the owning frame. */
export async function executeSidebarRequest(
  value: unknown,
  refresh: () => Promise<ServerNavigation>,
  run: (command: ServerCommand) => void | Promise<void>,
): Promise<unknown> {
  if (!value || typeof value !== "object")
    throw new Error("Invalid sidebar request.");
  const request = value as SidebarRpcRequest;
  if (request.action === "refresh") return refresh();
  if (request.action === "command") {
    if (
      !isServerCommand(request.command) ||
      !["prepare-chat", "mark-unread"].includes(request.command.action)
    )
      throw new Error("Invalid sidebar command.");
    return run(request.command);
  }
  if (
    request.action !== "post" ||
    !sidebarPaths.includes(request.path!) ||
    !request.body ||
    typeof request.body !== "object" ||
    Array.isArray(request.body)
  )
    throw new Error("Invalid sidebar request.");
  const options = request.options;
  if (
    options &&
    (Object.keys(options).some(
      (key) => !["timeoutMs", "requestId"].includes(key),
    ) ||
      (options.timeoutMs !== undefined &&
        (!Number.isFinite(options.timeoutMs) ||
          options.timeoutMs <= 0 ||
          options.timeoutMs > 30000)) ||
      (options.requestId !== undefined &&
        typeof options.requestId !== "string"))
  )
    throw new Error("Invalid sidebar request options.");
  const result = await post(request.path!, request.body, options);
  return request.path === "/api/organization"
    ? sidebarAgent(result as Agent)
    : result;
}
