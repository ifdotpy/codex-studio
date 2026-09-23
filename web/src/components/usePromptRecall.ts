import { useEffect, useRef, type KeyboardEvent } from "react";
import type { Message } from "../types";

// Recall text into the composer. The transcript's scroll position is independent.
export function usePromptRecall(
  scope: string,
  messages: Message[],
  draft: string,
  setDraft: (text: string) => void,
  history?: {
    before: string | null;
    load: (
      before: string,
    ) => Promise<{ messages: Message[]; before: string | null }>;
    onError: (error: unknown) => void;
  },
) {
  const cursor = useRef<{
    scope: string;
    entries: Message[];
    before: string | null;
    loading: boolean;
    request: number;
    index: number;
    value: string;
  } | null>(null);
  const latest = useRef({ scope, draft, setDraft });
  latest.current = { scope, draft, setDraft };
  const eligible = (items: Message[]) =>
    items.filter(
      (item) => item.role === "user" && !item.pending && item.text.trim(),
    );
  const reset = () => {
    cursor.current = null;
  };
  useEffect(() => {
    cursor.current = null;
    return () => {
      cursor.current = null;
    };
  }, [scope]);
  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (
      event.defaultPrevented ||
      event.nativeEvent.isComposing ||
      event.altKey ||
      event.ctrlKey ||
      event.metaKey ||
      event.shiftKey ||
      !["ArrowUp", "ArrowDown"].includes(event.key)
    )
      return false;
    if (cursor.current?.scope !== scope || cursor.current.value !== draft)
      reset();
    if (!cursor.current) {
      if (draft.length || event.key !== "ArrowUp") return false;
      const entries = eligible(messages);
      if (!entries.length && !history?.before) return false;
      cursor.current = {
        scope,
        entries,
        index: entries.length,
        value: draft,
        before: history?.before || null,
        loading: false,
        request: 0,
      };
    }
    event.preventDefault();
    const current = cursor.current;
    if (event.key === "ArrowDown") {
      current.request++;
      current.loading = false;
    }
    if (current.loading) return true;
    if (
      event.key === "ArrowUp" &&
      current.index === 0 &&
      current.before &&
      history
    ) {
      current.loading = true;
      const request = ++current.request;
      const valid = () =>
        cursor.current === current &&
        current.request === request &&
        latest.current.scope === scope &&
        latest.current.draft === current.value;
      void (async () => {
        try {
          while (current.before && valid()) {
            const before = current.before;
            const page = await history.load(before);
            if (!valid()) return;
            if (page.before === before)
              throw new Error("Message history did not advance. Try again.");
            const known = new Set(current.entries.map((item) => item.id));
            const earlier = eligible(page.messages).filter(
              (item) => !known.has(item.id),
            );
            current.before = page.before;
            if (!earlier.length) continue;
            current.entries = [...earlier, ...current.entries];
            current.index = earlier.length - 1;
            current.value = current.entries[current.index].text;
            latest.current.setDraft(current.value);
            return;
          }
        } catch (error) {
          if (valid()) history.onError(error);
        } finally {
          if (current.request === request) current.loading = false;
        }
      })();
      return true;
    }
    current.index = Math.max(
      0,
      Math.min(
        current.entries.length,
        current.index + (event.key === "ArrowUp" ? -1 : 1),
      ),
    );
    current.value = current.entries[current.index]?.text || "";
    setDraft(current.value);
    if (current.index === current.entries.length) reset();
    return true;
  };
  return { onKeyDown, reset };
}
