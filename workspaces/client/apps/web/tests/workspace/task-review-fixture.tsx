import { required } from "../teams/helpers.ts";
import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Tasks from "../../src/components/shell/Tasks.tsx";
createRoot(required(document.getElementById("root"))).render(
  <MantineProvider>
    <main style={{ maxWidth: 1100, margin: "0 auto", padding: 8 }}>
      <h1>Tasks</h1>
      <Tasks agentId="worker" rootId="lead" />
    </main>
  </MantineProvider>,
);
