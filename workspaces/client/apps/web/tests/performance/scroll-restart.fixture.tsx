import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { useConversationScroll } from "../../src/components/useConversationScroll.ts";
export function mount() {
  createRoot(document.body).render(
    React.createElement(function Harness() {
      const [id, select] = useState("workspace-a:agent:lead");
      const [ready, setReady] = useState(false);
      const [revision, redraw] = useState(0);
      const position = useConversationScroll(id, ready);
      window.scrollRestartFixture = { select, setReady, position, redraw };
      return React.createElement(
        "div",
        {
          id: "scroll",
          "data-revision": revision,
          ref: position.scroll,
          onScroll: position.onScroll,
          style: { height: 300, overflow: "auto" },
        },
        React.createElement(
          "div",
          { ref: position.content },
          ...Array.from({ length: ready ? 80 : 0 }, (_, index) =>
            React.createElement(
              "p",
              {
                key: index,
                "data-message": "message-" + index,
                style: { height: 60, margin: 0 },
              },
              "Message " + index,
            ),
          ),
        ),
      );
    }),
  );
}

declare global {
  interface Window {
    scrollRestartFixture: {
      select: React.Dispatch<React.SetStateAction<string>>;
      setReady: React.Dispatch<React.SetStateAction<boolean>>;
      position: ReturnType<typeof useConversationScroll>;
      redraw: React.Dispatch<React.SetStateAction<number>>;
    };
    scrollRestartFollowChanges: number;
    scrollRestartFollowUnsubscribe: () => void;
    scrollRestartMessageMeasurements: number;
  }
}
