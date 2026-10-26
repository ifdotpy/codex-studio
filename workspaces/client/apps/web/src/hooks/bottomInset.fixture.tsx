const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");
import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import { theme } from "/src/theme.ts";
import { useMobileViewport } from "/src/hooks/mobileViewport.ts";
import { BottomInsetBand, PassiveUsage } from "/src/hooks/BottomInsetBand.tsx";
import "@mantine/core/styles.css";
import "/src/style.css";
import "/src/components/Usage.css";
import "/src/hooks/viewport-surface.css";
import "/src/layout/layout-contract.generated.css";
const e = React.createElement;
function Fixture() {
  useMobileViewport(true);
  return e(
    MantineProvider,
    { theme },
    e(
      BottomInsetBand,
      null,
      e(
        "aside",
        {
          id: "sidebar",
          "data-presentation": "inline",
          style: { width: 226, background: "var(--sidebar)" },
        },
        "Chats",
      ),
      e(
        "main",
        {
          className: "workspace",
          style: { display: "flex", flexDirection: "column", flex: 1 },
        },
        e("div", { style: { flex: 1 } }, "Conversation"),
        e(
          "form",
          { id: "composer", style: { height: 100, flexShrink: 0 } },
          e("textarea", { id: "message", defaultValue: "Draft" }),
          e("button", null, "Send"),
        ),
        e(
          "div",
          { className: "usage-footer", style: { flex: "0 0 auto" } },
          e(
            PassiveUsage,
            null,
            e(
              "div",
              { className: "session-cost-summary" },
              "Session estimate: $3,589.76",
            ),
            e("span", { className: "token-rate" }, "42 tok/s"),
          ),
          e("button", { className: "context-use" }, "Context 39%"),
          e("button", null, "Account"),
          e("button", null, "Limits"),
        ),
      ),
    ),
  );
}
createRoot(fixtureRoot).render(e(Fixture));
