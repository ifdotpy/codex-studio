import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { useMessages } from "../hooks";
import {
  useConversationScroll,
  TRANSCRIPT_HISTORY_START_PX,
} from "../components/useConversationScroll";
declare global {
  interface Window {
    syncWindow: { historyRoot: Root };
  }
}
const workspaceId = document.documentElement.dataset.workspaceId ?? "";
function History() {
  const history = useMessages("open", "agent", true, "fixture", workspaceId);
  const scrolling = useConversationScroll("window-check", history.loaded);
  return React.createElement(
    "section",
    null,
    React.createElement(
      "button",
      { onClick: history.older, disabled: !history.before },
      "Older history",
    ),
    React.createElement(
      "div",
      {
        id: "history-scroll",
        ref: scrolling.scroll,
        style: { height: "100px", overflowY: "auto" },
        onScroll: () => {
          scrolling.onScroll();
          if (
            history.before &&
            !history.pageLoading &&
            (scrolling.scroll.current?.scrollTop ?? Infinity) <=
              TRANSCRIPT_HISTORY_START_PX &&
            scrolling.consumeHistoryScrollInput()
          )
            void history.older();
        },
      },
      React.createElement(
        "div",
        { ref: scrolling.content },
        history.items.map((item) =>
          React.createElement(
            "div",
            {
              key: item.id,
              "data-message": item.id,
              style: { height: "20px" },
            },
            item.text,
          ),
        ),
      ),
    ),
    React.createElement(
      "output",
      { "data-count": history.items.length },
      history.items.map((item) => item.id).join(","),
    ),
  );
}
const main = document.querySelector("main");
if (!main) throw new Error("Missing history fixture main");
window.syncWindow = { historyRoot: createRoot(main) };
window.syncWindow.historyRoot.render(React.createElement(History));
