import { randomUUID } from "../../../uuid";
import {
  ApiError,
  errorText,
  type PostBody,
  type PostResult,
  type ServerApi,
} from "../../../api";
import type { Json } from "../../../types";

/** A provider adapter sets `canMoveToBackground` only while its native control can act. */
export const canMoveToBackground = (p: Json, state: string) =>
  state === "running" &&
  p.type === "commandExecution" &&
  p.canMoveToBackground === true &&
  typeof p.id === "string" &&
  p.id.length > 0;

/** The provider task that keeps running after the command moved to the background. */
export const backgroundTaskId = (p: Json) =>
  typeof p.backgroundTaskId === "string" && p.backgroundTaskId
    ? p.backgroundTaskId
    : undefined;

export type BackgroundRequest = PostBody<"/api/processes/action">;
type BackgroundResult = PostResult<"/api/processes/action">;
type Api = Pick<ServerApi, "get" | "post">;

export type BackgroundOutcome =
  | { kind: "backgrounded" }
  | { kind: "notRunning" }
  | { kind: "failed"; message: string }
  /** Keep the request. Only the same request ID may be sent again. */
  | { kind: "uncertain"; request: BackgroundRequest; message: string };

const UNKNOWN = "The result is unknown. Check the result before you try again.";

function outcome(
  result: BackgroundResult,
  request: BackgroundRequest,
): BackgroundOutcome {
  const response = result.response || {};
  const status = typeof response.status === "string" ? response.status : "";
  if (result.status === "uncertain" || status === "uncertain")
    return { kind: "uncertain", request, message: UNKNOWN };
  if (status === "backgrounded") return { kind: "backgrounded" };
  if (status === "not_running") return { kind: "notRunning" };
  return {
    kind: "failed",
    message:
      (typeof response.error === "string" && response.error) ||
      result.error ||
      "The command did not move to the background.",
  };
}

async function submit(
  api: Api,
  request: BackgroundRequest,
): Promise<BackgroundOutcome> {
  try {
    return outcome(
      await api.post("/api/processes/action", request, { timeoutMs: 30000 }),
      request,
    );
  } catch (error) {
    if (error instanceof ApiError && error.status >= 400 && error.status < 500)
      return { kind: "failed", message: errorText(error) };
    return { kind: "uncertain", request, message: UNKNOWN };
  }
}

/** Ask the server to move one running command of the agent's current turn. */
export async function requestCommandBackground(
  api: Api,
  agentId: string,
  turnId: string | undefined,
  itemId: string,
): Promise<BackgroundOutcome> {
  let target;
  try {
    target = (await api.get("/api/processes", { query: { agent: agentId } }))
      .responseTarget;
  } catch (error) {
    return { kind: "failed", message: errorText(error) };
  }
  if (!target || (turnId && target.turnId !== turnId))
    return {
      kind: "failed",
      message: "This command is no longer in the current response.",
    };
  return submit(api, {
    action: "background_command",
    requestId: randomUUID(),
    agent: agentId,
    accountKey: target.accountKey,
    epoch: target.epoch,
    responseTarget: target,
    itemId,
  });
}

/**
 * Read the saved receipt of an earlier request. When the server has no receipt, send
 * the same request again: its request ID lets the server apply it at most once.
 */
export async function checkCommandBackground(
  api: Api,
  request: BackgroundRequest,
): Promise<BackgroundOutcome> {
  let receipt;
  try {
    receipt = await api.get("/api/processes/action", {
      query: {
        requestId: request.requestId,
        agent: request.agent,
        accountKey: request.accountKey,
        epoch: request.epoch,
      },
    });
  } catch {
    return { kind: "uncertain", request, message: UNKNOWN };
  }
  if (!receipt.found || !receipt.result) return submit(api, request);
  return outcome(receipt.result, request);
}
