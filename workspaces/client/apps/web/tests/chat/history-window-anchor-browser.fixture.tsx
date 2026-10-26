import React, { useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import HistoryWindow from "../../src/components/conversation/transcript/HistoryWindow.tsx";
import type { HistoryGroup } from "../../src/components/turnHistoryModel.ts";
declare global {
  interface Window {
    __lastInput?: number;
    scrollToTopAsUser: () => void;
    scrollToMessage: (index: number) => void;
    commitViewportAnchor: () => void;
    prependPage: (start: number) => void;
    appendForwardPage: () => void;
    jumpToOldest: () => void;
    releaseScroll: () => void;
  }
}
const params = new URLSearchParams(location.search);
const start = Number(params.get("start")),
  count = Number(params.get("count"));
const disableNativeAnchoring = params.get("disableNativeAnchoring") === "true";
const makeRows = (start: number, count: number): HistoryGroup[] =>
  Array.from({ length: count }, (_, n) => {
    const id = "message-" + (start + n);
    return {
      id,
      items: [
        {
          id,
          role: "user",
          text: "A short fixed-height transcript row",
          turnId: id,
        },
      ],
    };
  });
function Harness() {
  const [rows, setRows] = useState(() => makeRows(start, count));
  const scroll = useRef<HTMLDivElement>(null);

  const hasRecentScrollInput = React.useCallback(
    () => performance.now() - (window.__lastInput ?? -Infinity) < 600,
    [],
  );
  window.scrollToTopAsUser = () => {
    const root = scroll.current!;
    root.dispatchEvent(new WheelEvent("wheel", { deltaY: -1 }));
    const holdScroll = (event: Event) => event.stopImmediatePropagation();
    root.addEventListener("scroll", holdScroll, true);
    root.scrollTop = 0;
    root.dispatchEvent(new Event("scroll"));
    root.removeEventListener("scroll", holdScroll, true);
  };
  window.scrollToMessage = (index) => {
    const root = scroll.current!;
    root.scrollTop = index * 72 - 16;
    root.dispatchEvent(new Event("scroll"));
  };
  window.commitViewportAnchor = () =>
    scroll.current!.dispatchEvent(new Event("scroll"));
  window.prependPage = (start) =>
    flushSync(() =>
      setRows((current) => [...makeRows(start, 120), ...current]),
    );
  window.appendForwardPage = () => {
    const root = scroll.current!;
    const holdScroll = (event: Event) => event.stopImmediatePropagation();
    root.addEventListener("scroll", holdScroll, true);
    flushSync(() => setRows((current) => [...current, ...makeRows(160, 60)]));
    root.scrollTop = root.scrollHeight;
    window.releaseScroll = () =>
      root.removeEventListener("scroll", holdScroll, true);
  };
  window.jumpToOldest = () => {
    const root = scroll.current!;
    root.dispatchEvent(new Event("studio-history-reset-anchor"));
    const holdScroll = (event: Event) => event.stopImmediatePropagation();
    root.addEventListener("scroll", holdScroll, true);
    root.scrollTop = 0;
    root.dispatchEvent(new Event("scroll"));
    root.removeEventListener("scroll", holdScroll, true);
    flushSync(() => setRows((current) => [...makeRows(0, 120), ...current]));
  };
  return (
    <div
      id="messages"
      ref={scroll}
      style={{
        height: 500,
        overflow: "auto",
        overflowAnchor: disableNativeAnchoring ? "none" : "auto",
        position: "relative",
      }}
    >
      <HistoryWindow
        rows={rows}
        scrollContainer={scroll}
        hasRecentScrollInput={hasRecentScrollInput}
        storageKey="history-window-anchor-audit"
        render={(row) => (
          <article
            data-message={row.items[0].id}
            style={{ height: 72, minHeight: 72, boxSizing: "border-box" }}
          >
            {row.items[0].text}
          </article>
        )}
      />
    </div>
  );
}
const root = document.querySelector("#root")!;
createRoot(root).render(<Harness />);
const messages = document.querySelector("#messages")!;
messages.addEventListener("wheel", () => {
  window.__lastInput = performance.now();
});
