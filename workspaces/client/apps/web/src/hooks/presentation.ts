import { useCallback, useLayoutEffect, useMemo, useRef } from "react";
import type { UsageAccount } from "../components/Usage";
import type { Json, Message, Snapshot, TranscriptWireItem } from "../types";
import {
  createChatSnapshotSelector,
  retainArray,
  retainUsageAccounts,
} from "./snapshotSelection";

// Event callbacks see the latest committed selection without invalidating
// consumers when another chat changes. Never call these during render.
export function useCommittedCallback<Args extends unknown[], Result>(
  callback: (...args: Args) => Result,
): (...args: Args) => Result {
  const current = useRef(callback);
  useLayoutEffect(() => {
    current.current = callback;
  });
  return useCallback((...args: Args) => current.current(...args), []);
}

export function useRetainedArray<T>(values: T[]): T[] {
  const previous = useRef(values);
  const selected = retainArray(previous.current, values);
  useLayoutEffect(() => {
    previous.current = selected;
  }, [selected]);
  return selected;
}

// A team change that leaves every account unchanged keeps the array identity.
export function useRetainedUsageAccounts(values: UsageAccount[]) {
  const previous = useRef(values);
  const selected = retainUsageAccounts(previous.current, values);
  useLayoutEffect(() => {
    previous.current = selected;
  }, [selected]);
  return selected;
}

export function useChatSnapshot(
  data: Snapshot | null,
  rootId?: string,
  includeUnassignedRequests = false,
) {
  const select = useMemo(
    () => createChatSnapshotSelector(includeUnassignedRequests),
    [includeUnassignedRequests],
  );
  return select(data, rootId);
}

// Name the sender of team events, so a list of events is not identical rows.
function eventSender(input: Json): string {
  if (input.kind !== "agent_message" && input.kind !== "child_result")
    return "";
  try {
    const event = JSON.parse(String(input.text || ""));
    const name = event?.sender_name || event?.name;
    if (typeof name !== "string" || !name.trim()) return "";
    return input.kind === "child_result"
      ? `Result from ${name.trim()}`
      : `Message from ${name.trim()}`;
  } catch {
    return "";
  }
}

// Native input batches have separate user-visible message identities.
export function transcriptMessages(
  source: TranscriptWireItem[],
  id: string | null,
): Message[] {
  return source.flatMap((m: TranscriptWireItem) =>
    m.inputs
      ? m.inputs.map((r, i) => ({
          ...m,
          threadId: m.threadId ?? undefined,
          id: r.id ? `${id}:${r.id}` : `${m.id}:${i}`,
          sourceId: m.id,
          clientMessageId:
            r.clientMessageId ||
            r.id ||
            (i === 0 ? m.clientMessageId : undefined),
          deliveryStatus: r.deliveryStatus,
          requestedDelivery: r.requestedDelivery,
          deliveryError: r.deliveryError,
          materialized: r.materialized ?? m.materialized,
          pending: r.pending ?? undefined,
          assets: r.assets || (i === 0 ? m.assets : []),
          role: r.kind === "user" ? "user" : "tool",
          text: r.text,
          truncated: r.truncated,
          title:
            r.kind === "user"
              ? "You"
              : eventSender(r) ||
                (
                  {
                    agent_message: "Agent message received",
                    child_result: "Worker result received",
                    monitor_exit: "Command finished",
                    monitor_cancelled: "Command cancelled",
                    complaint_response: "Complaint response received",
                    followup: "Agent follow-up",
                    complaint: "Complaint requires a response",
                    work_released: "Task released from a failed worker",
                  } as Record<string, string>
                )[r.kind] ||
                "Team activity",
        }))
      : [{ ...m, threadId: m.threadId ?? undefined }],
  );
}
