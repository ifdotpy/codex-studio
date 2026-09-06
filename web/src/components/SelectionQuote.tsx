import { Button } from "@mantine/core";
import { Quote } from "lucide-react";
import { useEffect, useRef, useState, type RefObject } from "react";
import "./selection-quote.css";

type Excerpt = { text: string; messageId: string; left: number; top: number };

// A range must belong to one message's prose, including both of its endpoints.
// Do not quote tool controls or the source and controls of an embedded preview.
export function selectedExcerpt(
  root: HTMLElement | null,
  visibleOnly = false,
): Excerpt | null {
  const selection = window.getSelection();
  if (
    !root ||
    !selection ||
    selection.isCollapsed ||
    selection.rangeCount !== 1
  )
    return null;
  const range = selection.getRangeAt(0);
  const element = (node: Node) =>
    node.nodeType === Node.ELEMENT_NODE
      ? (node as Element)
      : node.parentElement;
  const start = element(range.startContainer)?.closest(".prose");
  const end = element(range.endContainer)?.closest(".prose");
  const message = start?.closest<HTMLElement>("[data-message]");
  if (!start || start !== end || !message || !root.contains(message))
    return null;
  for (const excluded of start.querySelectorAll(
    ".rich-preview, button, input, textarea, select, iframe, [contenteditable]",
  )) {
    if (range.intersectsNode(excluded)) return null;
  }
  const text = selection.toString();
  if (!text.trim()) return null;
  const rect = Array.from(range.getClientRects()).at(-1);
  if (!rect) return null;
  const bounds = root.getBoundingClientRect();
  if (visibleOnly && (rect.bottom < bounds.top || rect.top > bounds.bottom))
    return null;
  const viewport = window.visualViewport;
  const width = viewport?.width || window.innerWidth;
  const height = viewport?.height || window.innerHeight;
  const x = viewport?.offsetLeft || 0;
  const y = viewport?.offsetTop || 0;
  return {
    text,
    messageId: message.dataset.message!,
    left: Math.max(x + 8, Math.min(rect.left, x + width - 180)),
    top: Math.max(y + 8, Math.min(rect.bottom + 8, y + height - 48)),
  };
}

export default function SelectionQuote({
  chatId,
  root,
  onQuote,
}: {
  chatId: string | null;
  root: RefObject<HTMLDivElement | null>;
  onQuote: (text: string) => void;
}) {
  const [excerpt, setExcerpt] = useState<Excerpt | null>(null);
  const button = useRef<HTMLButtonElement>(null);
  const callback = useRef(onQuote);
  callback.current = onQuote;
  useEffect(() => {
    setExcerpt(null);
    let current: Excerpt | null = null;
    const update = () => {
      // A keyboard focus move to the quote action must preserve its excerpt.
      if (document.activeElement === button.current && current) return;
      current = selectedExcerpt(root.current, true);
      setExcerpt(current);
    };
    const keydown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        current = null;
        setExcerpt(null);
      } else if (event.altKey && event.shiftKey && event.code === "KeyQ") {
        const selected = selectedExcerpt(root.current);
        if (!selected) return;
        event.preventDefault();
        callback.current(selected.text);
        window.getSelection()?.removeAllRanges();
        current = null;
        setExcerpt(null);
      }
    };
    document.addEventListener("selectionchange", update);
    document.addEventListener("pointerup", update);
    document.addEventListener("keydown", keydown);
    window.addEventListener("scroll", update, true);
    window.addEventListener("resize", update);
    window.visualViewport?.addEventListener("resize", update);
    return () => {
      document.removeEventListener("selectionchange", update);
      document.removeEventListener("pointerup", update);
      document.removeEventListener("keydown", keydown);
      window.removeEventListener("scroll", update, true);
      window.removeEventListener("resize", update);
      window.visualViewport?.removeEventListener("resize", update);
    };
  }, [chatId, root]);
  if (!excerpt) return null;
  return (
    <Button
      ref={button}
      className="selection-quote"
      style={{ left: excerpt.left, top: excerpt.top }}
      size="compact-sm"
      leftSection={<Quote size={15} />}
      aria-keyshortcuts="Alt+Shift+Q"
      title="Quote selection (Alt+Shift+Q)"
      onPointerDown={(event) => event.preventDefault()}
      onClick={() => {
        onQuote(excerpt.text);
        window.getSelection()?.removeAllRanges();
        setExcerpt(null);
      }}
    >
      Quote selection
    </Button>
  );
}
