import { useEffect, useMemo, useState } from "react";
import { useServerApi } from "../../servers/ServerServicesContext";
import type { components } from "../../generated/api";
import type { Message } from "../../types";
import {
  isPendingQuestionRequest,
  pendingQuestionRequests,
  questionCard,
  questionHistoryIndex,
  questionRefreshKey,
  type QuestionHistoryEntry,
  type QuestionLookup,
} from "./questionCardModel";

type RequestDto = components["schemas"]["RequestEntityDto"];

/**
 * Question cards for one chat. Pending state comes from live request entities;
 * answers come from the question history, read again when a question changes.
 */
export function useQuestionCards(
  agentId: string | undefined,
  scope: string,
  items: Message[],
  requests: RequestDto[],
) {
  const { get } = useServerApi();
  const refreshKey = useMemo(
    () => questionRefreshKey(items, requests),
    [items, requests],
  );
  const owner = `${scope}\u0000${agentId || ""}`;
  const [history, setHistory] = useState<{
    owner: string;
    entries: QuestionHistoryEntry[];
  }>({ owner, entries: [] });
  useEffect(() => {
    if (!agentId) return;
    let active = true;
    get("/api/questions", { query: { agent: agentId } })
      .then((data) => {
        if (active) setHistory({ owner, entries: data.items });
      })
      .catch(() => {
        /* Cards still render from the stored items; answers stay unknown. */
      });
    return () => {
      active = false;
    };
  }, [owner, refreshKey]);
  const entries = history.owner === owner ? history.entries : undefined;
  const lookup = useMemo<QuestionLookup>(
    () => ({
      agentId,
      history: questionHistoryIndex(entries || []),
      pending: pendingQuestionRequests(requests),
      pendingIds: new Set(
        requests.filter(isPendingQuestionRequest).map((request) => request.id),
      ),
    }),
    // Requests change identity often; the refresh key names their question state.
    [agentId, entries, refreshKey],
  );
  return useMemo(() => {
    const cache = new WeakMap<Message, ReturnType<typeof questionCard>>();
    return (item: Message) => {
      if (cache.has(item)) return cache.get(item);
      const card = questionCard(item, lookup);
      cache.set(item, card);
      return card;
    };
  }, [lookup]);
}
