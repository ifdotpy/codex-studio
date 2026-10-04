import { useState } from "react";
import { post, ApiError } from "./api";
import type { PostBody } from "./api";
import type { Agent } from "./types";
type Request = Omit<PostBody<"/api/action">, "action"> & {
  action: "compact" | "review";
};
type Action = Request["action"];

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object";
}

function isActionRequest(value: unknown): value is Request {
  if (!isRecord(value)) return false;
  const request = value;
  if (
    typeof request.id !== "string" ||
    typeof request.request_id !== "string" ||
    !request.request_id ||
    (request.action !== "compact" && request.action !== "review") ||
    !isRecord(request.context)
  )
    return false;
  const context = request.context;
  return (
    typeof context.accountKey === "string" &&
    (context.threadId === undefined || typeof context.threadId === "string") &&
    (context.epoch === undefined || typeof context.epoch === "number")
  );
}

export function isDefinitivelyNotApplied(error: unknown): boolean {
  if (
    !(error instanceof ApiError) ||
    !error.details ||
    typeof error.details !== "object"
  )
    return false;
  return "outcome" in error.details && error.details.outcome === "not_applied";
}

export function useNativeAction(
  workspace: string | undefined,
  workspaceId: string | undefined,
  notify: (text: string) => void,
) {
  const [, changed] = useState(0);
  const [active, setActive] = useState(false);
  const key = (id: string) => `studio-native-action:${workspace}:${id}`;
  const read = (id: string): Request | null => {
    if (!workspace) return null;
    const raw = localStorage.getItem(key(id));
    if (!raw) return null;
    const request: unknown = JSON.parse(raw);
    if (!isActionRequest(request) || request.id !== id)
      throw new Error(
        "The saved action request is invalid. Restore browser storage before this action.",
      );
    return request;
  };
  const submit = async (agent: Agent, action: Action) => {
    if (!workspace) throw new Error("Wait for the workspace to load.");
    const existing = read(agent.id);
    if (existing && existing.action !== action)
      throw new Error("Check the saved action request before another action.");
    const request: Request = existing || {
      id: agent.id,
      action,
      request_id: crypto.randomUUID(),
      context: {
        accountKey: agent.accountKey || "default",
        threadId: agent.threadId,
        epoch: agent.epoch,
      },
    };
    // Storage must succeed before HTTP. Keep the exact account, thread and epoch on retry.
    localStorage.setItem(key(agent.id), JSON.stringify(request));
    changed((value) => value + 1);
    setActive(true);
    const acknowledge = () => {
      if (read(agent.id)?.request_id === request.request_id)
        localStorage.removeItem(key(agent.id));
      changed((value) => value + 1);
    };
    try {
      const response = await post("/api/action", request, {
        workspaceId,
        timeoutMs: 15000,
      });
      if (response.receipt?.requestId !== request.request_id)
        throw new Error(
          "The action reply has no matching receipt. Check the saved request.",
        );
      acknowledge();
      const resultError =
        response.result &&
        typeof response.result === "object" &&
        !Array.isArray(response.result) &&
        "error" in response.result &&
        typeof response.result.error === "string"
          ? response.result.error
          : undefined;
      const error = response.error || response.outcome?.error || resultError;
      if (error) notify(error);
      else if (response.replayed)
        notify("This action request was already saved. No new action starts.");
      return response;
    } catch (error) {
      if (isDefinitivelyNotApplied(error)) acknowledge();
      throw error;
    } finally {
      setActive(false);
    }
  };
  const pending = (id: string) => {
    try {
      return read(id);
    } catch {
      return null;
    }
  };
  return { pending, submit, active };
}
