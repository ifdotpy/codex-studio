import { useState } from "react";
import { api, ApiError } from "./api";
import type { Agent } from "./types";

type Action = "compact" | "review";
type Request = {
  id: string;
  action: Action;
  request_id: string;
  context: { accountKey: string; threadId?: string; epoch?: number };
};

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
    const request = JSON.parse(raw);
    if (
      request.id !== id ||
      !request.request_id ||
      !["review", "compact"].includes(request.action)
    )
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
    const request = existing || {
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
      const response = await api("/api/action", request, {
        workspaceId,
        timeoutMs: 15000,
      });
      if (response.receipt?.requestId !== request.request_id)
        throw new Error(
          "The action reply has no matching receipt. Check the saved request.",
        );
      acknowledge();
      if (response.error || response.outcome?.error || response.result?.error)
        notify(
          response.error || response.outcome?.error || response.result.error,
        );
      else if (response.replayed)
        notify("This action request was already saved. No new action starts.");
      return response;
    } catch (error) {
      if (
        error instanceof ApiError &&
        (error.details as { outcome?: string })?.outcome === "not_applied"
      )
        acknowledge();
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
