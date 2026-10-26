const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider, Menu } from "@mantine/core";
import { EdgeRail } from "/src/components/edge-rail/EdgeRail.tsx";
import { useEdgeRail } from "/src/components/edge-rail/useEdgeRail.ts";
import { theme } from "/src/theme.ts";
import "@mantine/core/styles.css";
import "/src/layout/layout-contract.generated.css";
const e = React.createElement;
function Fixture() {
  const geometry = useEdgeRail();
  const [last, setLast] = React.useState("None");
  return e(
    MantineProvider,
    { theme },
    e(
      "main",
      null,
      e("h1", null, "Conversation title"),
      e("output", { id: "last-action" }, last),
    ),
    e(EdgeRail, {
      geometry,
      sidebar: {
        expanded: true,
        onClick: () => setLast("Toggle conversations"),
      },
      messages: { count: 3, onClick: () => setLast("Messages") },
      team: { active: 1, total: 3, onClick: () => setLast("Team") },
      menu: (
        <Menu.Item onClick={() => setLast("Existing action")}>
          Existing action
        </Menu.Item>
      ),
    }),
  );
}
createRoot(fixtureRoot).render(e(Fixture));
