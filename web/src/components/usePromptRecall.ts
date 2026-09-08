import { useEffect, useRef, type KeyboardEvent } from "react";
import type { Message } from "../types";

// Recall text into the composer. The transcript's scroll position is independent.
export function usePromptRecall(
  scope: string,
  messages: Message[],
  draft: string,
  setDraft: (text: string) => void,
) {
  const cursor = useRef<{
    scope: string;
    texts: string[];
    index: number;
    value: string;
  } | null>(null);
  const reset = () => {
    cursor.current = null;
  };
  useEffect(() => {
    cursor.current = null;
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
      const texts = messages
        .filter(
          (item) => item.role === "user" && !item.pending && item.text.trim(),
        )
        .map((item) => item.text);
      if (!texts.length) return false;
      cursor.current = { scope, texts, index: texts.length, value: draft };
    }
    event.preventDefault();
    const current = cursor.current;
    current.index = Math.max(
      0,
      Math.min(
        current.texts.length,
        current.index + (event.key === "ArrowUp" ? -1 : 1),
      ),
    );
    current.value = current.texts[current.index] || "";
    setDraft(current.value);
    if (current.index === current.texts.length) reset();
    return true;
  };
  return { onKeyDown, reset };
}
