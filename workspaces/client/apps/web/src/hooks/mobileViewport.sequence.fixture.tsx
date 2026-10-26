const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");
import React from "react";
import { createRoot } from "react-dom/client";
import { useMobileViewport } from "/src/hooks/mobileViewport.ts";
import "/src/hooks/viewport-surface.css";
import "/src/layout/layout-contract.generated.css";
const e = React.createElement;
function Fixture() {
  useMobileViewport(true);
  return e(
    React.Fragment,
    null,
    e(
      "aside",
      { id: "sidebar", style: { width: 226, height: "100%", flex: "none" } },
      "Chats",
    ),
    e(
      "section",
      {
        style: {
          height: "100%",
          flex: 1,
          display: "flex",
          flexDirection: "column",
        },
      },
      e("header", { style: { height: 44, flex: "none" } }, "Header"),
      e(
        "main",
        {
          id: "conversation",
          style: {
            display: "flex",
            flex: 1,
            minHeight: 0,
            flexDirection: "column",
          },
        },
        e(
          "div",
          {
            id: "messages",
            style: { flex: 1, minHeight: 0, overflow: "auto" },
          },
          "Transcript ".repeat(100),
        ),
        e(
          "form",
          {
            id: "composer",
            style: {
              height: ["device final", "product target"].includes(
                window.sequenceCase,
              )
                ? 91
                : 120,
              flex: "none",
              margin: 0,
              marginBottom: window.sequenceCase === "product target" ? 24 : 0,
            },
          },
          e("textarea", { id: "message", defaultValue: "Draft" }),
        ),
      ),
    ),
  );
}
createRoot(fixtureRoot).render(e(Fixture));
