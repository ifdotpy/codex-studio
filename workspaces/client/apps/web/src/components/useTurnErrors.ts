import { useEffect, useMemo, useState } from "react";
import { get } from "../api";
import type { Agent, JsonValue, Message } from "../types";

const cachedErrors = new Map<string, Record<string, JsonValue>>();
const inFlight = new Map<string, Promise<Record<string, JsonValue>>>();

export function failedTurnsNeedingLookup(
  items: Pick<Message, "turnId" | "turnStatus">[],
  existing: Set<string>,
): string[] {
  return [
    ...new Set(
      items.flatMap((item) =>
        item.turnStatus === "failed" &&
        item.turnId &&
        typeof item.turnId === "string" &&
        !existing.has(item.turnId)
          ? [item.turnId]
          : [],
      ),
    ),
  ];
}

// Old servers saved terminal errors in analytics without transcript notices.
// The compact view also works with their full analytics response.
export function useTurnErrors(items: Message[], agent?: Agent, workspace = "") {
  const scope = `${workspace}:${agent?.id}:${agent?.threadId}`;
  const [result, setResult] = useState<{
    scope: string;
    checked: string[];
    failed: string[];
    errors: Record<string, JsonValue>;
  }>({ scope, checked: [], failed: [], errors: {} });
  const current = useMemo(
    () =>
      result.scope === scope
        ? result
        : { scope, checked: [], failed: [], errors: {} },
    [result, scope],
  );
  const cached = cachedErrors.get(scope);
  const { errors, resolved, existing, turns } = useMemo(() => {
    const errors = { ...cached, ...current.errors };
    const resolved = new Set<string>();
    for (const item of items) {
      if (item.turnErrorResolved && item.turnId) resolved.add(item.turnId);
      if (item.turnError && item.turnId) errors[item.turnId] = item.turnError;
    }
    const latest = agent?.nativeTurnError;
    if (latest?.turnId && latest.error) errors[latest.turnId] = latest.error;
    else if (
      agent?.status === "failed" &&
      !agent.turnId &&
      agent.lastCompletedTurn &&
      agent.error
    )
      errors[agent.lastCompletedTurn] = agent.error;
    const existing = new Set(
      items.flatMap((item) =>
        item.nativeNotice === "error" && typeof item.turnId === "string"
          ? [item.turnId]
          : [],
      ),
    );
    const turns = failedTurnsNeedingLookup(items, existing);
    return { errors, resolved, existing, turns };
  }, [
    items,
    cached,
    current.errors,
    agent?.nativeTurnError,
    agent?.status,
    agent?.turnId,
    agent?.lastCompletedTurn,
    agent?.error,
  ]);
  const pending = turns
    .filter(
      (turn) =>
        !errors[turn] && !resolved.has(turn) && !current.checked.includes(turn),
    )
    .slice(-120);
  const requestKey = pending.join(",");
  useEffect(() => {
    if (!agent?.id || !agent.threadId || !requestKey) return;
    let active = true;
    const requested = requestKey.split(",");
    const agentId = agent.id;
    const threadId = agent.threadId;
    const key = `${scope}:${requestKey}`;
    const existingRequest = inFlight.get(key);
    let request: Promise<Record<string, JsonValue>>;
    if (existingRequest) request = existingRequest;
    else {
      request = get("/api/analytics", {
        query: {
          agent: agentId,
          scope: "agent",
          view: "turn-errors",
          thread: threadId,
          turns: requestKey,
          limit: "1",
        },
        timeoutMs: 60000,
      }).then((data) => {
        const recovered: Record<string, JsonValue> = {};
        for (const turn of data.turns || []) {
          if (
            turn.agentId === agentId &&
            turn.threadId === threadId &&
            typeof turn.turnId === "string" &&
            requested.includes(turn.turnId) &&
            turn.status === "failed" &&
            turn.error
          )
            recovered[turn.turnId] = turn.error;
        }
        const previous = cachedErrors.get(scope);
        cachedErrors.delete(scope);
        cachedErrors.set(
          scope,
          Object.fromEntries(
            Object.entries({ ...previous, ...recovered }).slice(-120),
          ),
        );
        while (cachedErrors.size > 24)
          cachedErrors.delete(cachedErrors.keys().next().value!);
        return recovered;
      });
      inFlight.set(key, request);
      void request.finally(() => inFlight.delete(key)).catch(() => {});
    }
    request
      .then((recovered) => {
        if (!active) return;
        setResult((old) => ({
          scope,
          checked: [...(old.scope === scope ? old.checked : []), ...requested],
          failed:
            old.scope === scope
              ? old.failed.filter((turn) => !requested.includes(turn))
              : [],
          errors: { ...(old.scope === scope ? old.errors : {}), ...recovered },
        }));
      })
      .catch(() => {
        if (active)
          setResult((old) => ({
            scope,
            checked: [
              ...(old.scope === scope ? old.checked : []),
              ...requested,
            ],
            failed: [...(old.scope === scope ? old.failed : []), ...requested],
            errors: old.scope === scope ? old.errors : {},
          }));
      });
    return () => {
      active = false;
    };
  }, [scope, agent?.id, agent?.threadId, requestKey]);
  const enriched = useMemo(() => {
    const last = new Map<string, number>();
    items.forEach((item, index) => {
      if (item.turnId && typeof item.turnId === "string")
        last.set(item.turnId, index);
    });
    if (!Object.keys(errors).length) return items;
    return items.flatMap((item, index): Message[] => {
      const turnId = item.turnId;
      if (!turnId || typeof turnId !== "string") return [item];
      const error = errors[turnId];
      if (!error || existing.has(turnId) || last.get(turnId) !== index)
        return [item];
      return [
        item,
        {
          id: `turn-error:${agent?.id}:${agent?.threadId}:${turnId}`,
          role: "system",
          text: "",
          turnId,
          turnStatus: "failed",
          nativeNotice: "error",
          nativeError: error,
        },
      ];
    });
  }, [items, errors, existing, agent?.id, agent?.threadId]);
  const loading = new Set(
    agent?.id && agent.threadId
      ? turns.filter(
          (turn) =>
            !errors[turn] &&
            !resolved.has(turn) &&
            !current.checked.includes(turn),
        )
      : [],
  );
  return {
    items: enriched,
    loading,
    failed: new Set(current.failed),
    retry: (turn: string) =>
      setResult((old) => ({
        scope,
        errors: old.scope === scope ? old.errors : {},
        checked:
          old.scope === scope ? old.checked.filter((id) => id !== turn) : [],
        failed:
          old.scope === scope ? old.failed.filter((id) => id !== turn) : [],
      })),
  };
}
