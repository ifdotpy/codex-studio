import React, { useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import HistoryWindow from "../../src/components/conversation/transcript/HistoryWindow.tsx";
import type { HistoryGroup } from "../../src/components/turnHistoryModel.ts";
declare global {
  interface Window {
    historyAnchorFixture: {
      __lastInput?: number;
      scrollToTopAsUser: () => void;
      scrollToMessage: (index: number) => void;
      commitViewportAnchor: () => void;
      prependPage: (start: number) => void;
      appendForwardPage: () => void;
      jumpToOldest: () => void;
      releaseScroll: () => void;
    };
  }
}
const params = new URLSearchParams(location.search);
const start = Number(params.get("start")),
  count = Number(params.get("count"));
const disableNativeAnchoring = params.get("disableNativeAnchoring") === "true";
const notMounted = () => {
  throw new Error("Fixture not mounted");
};
window.historyAnchorFixture = {
  scrollToTopAsUser: notMounted,
  scrollToMessage: notMounted,
  commitViewportAnchor: notMounted,
  prependPage: notMounted,
  appendForwardPage: notMounted,
  jumpToOldest: notMounted,
  releaseScroll: notMounted,
};
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
  const scrollRoot = () => {
    if (!scroll.current) throw new Error("Missing scroll root");
    return scroll.current;
  };
  const hasRecentScrollInput = React.useCallback(
    () =>
      performance.now() -
        (window.historyAnchorFixture.__lastInput ?? -Infinity) <
      600,
    [],
  );
  window.historyAnchorFixture.scrollToTopAsUser = () => {
    const root = scrollRoot();
    root.dispatchEvent(new WheelEvent("wheel", { deltaY: -1 }));
    const holdScroll = (event: Event) => event.stopImmediatePropagation();
    root.addEventListener("scroll", holdScroll, true);
    root.scrollTop = 0;
    root.dispatchEvent(new Event("scroll"));
    root.removeEventListener("scroll", holdScroll, true);
  };
  window.historyAnchorFixture.scrollToMessage = (index) => {
    const root = scrollRoot();
    root.scrollTop = index * 72 - 16;
    root.dispatchEvent(new Event("scroll"));
  };
  window.historyAnchorFixture.commitViewportAnchor = () =>
    scrollRoot().dispatchEvent(new Event("scroll"));
  window.historyAnchorFixture.prependPage = (start) =>
    flushSync(() =>
      setRows((current) => [...makeRows(start, 120), ...current]),
    );
  window.historyAnchorFixture.appendForwardPage = () => {
    const root = scrollRoot();
    const holdScroll = (event: Event) => event.stopImmediatePropagation();
    root.addEventListener("scroll", holdScroll, true);
    flushSync(() => setRows((current) => [...current, ...makeRows(160, 60)]));
    root.scrollTop = root.scrollHeight;
    window.historyAnchorFixture.releaseScroll = () =>
      root.removeEventListener("scroll", holdScroll, true);
  };
  window.historyAnchorFixture.jumpToOldest = () => {
    const root = scrollRoot();
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
const root = document.querySelector("#root");
if (!root) throw new Error("Missing fixture root");
createRoot(root).render(<Harness />);
document.querySelector("#messages")?.addEventListener("wheel", () => {
  window.historyAnchorFixture.__lastInput = performance.now();
});
