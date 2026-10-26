import { useEffect, useMemo, useState } from "react";
import { useServerSync } from "../../servers/ServerServicesContext";
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
 * answers come from source-published question history entities.
 */
export function useQuestionCards(
  agentId: string | undefined,
  scope: string,
  items: Message[],
  requests: RequestDto[],
) {
  const { subscribeStateProjection } = useServerSync();
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
    let source: unknown;
    let familyKey = "";
    return subscribeStateProjection(
      (snapshot) => {
        if (!snapshot) return;
        const agent = snapshot.runtime.agents.find((row) => row.id === agentId);
        const root = agent?.rootId || agentId;
        const family = new Set(
          snapshot.runtime.agents
            .filter((row) => (row.rootId || row.id) === root)
            .map((row) => row.id),
        );
        const nextFamily = JSON.stringify([...family].sort());
        if (
          source === snapshot.runtime.questionHistory &&
          familyKey === nextFamily
        )
          return;
        source = snapshot.runtime.questionHistory;
        familyKey = nextFamily;
        setHistory({
          owner,
          entries: (snapshot.runtime.questionHistory ?? []).filter((entry) =>
            family.has(entry.agent),
          ),
        });
      },
      () => {},
    );
  }, [owner, subscribeStateProjection]);
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
