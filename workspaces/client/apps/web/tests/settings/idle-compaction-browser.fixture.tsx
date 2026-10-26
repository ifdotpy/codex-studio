const fixtureRoot = document.getElementById("root");
if (!fixtureRoot) throw new Error("Missing fixture root");
import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { OrchestrationSettings } from "../../src/components/OrchestrationSettings.tsx";
import { IdleCompactionNotice } from "../../src/components/IdleCompactionNotice.tsx";
import "../../src/components/ui/primitives.css";
createRoot(fixtureRoot).render(
  <MantineProvider>
    <main style={{ padding: 16, maxWidth: 900, margin: "auto" }}>
      <OrchestrationSettings active={true} />
      <section aria-label="Transcript">
        <h2>Transcript</h2>
        <IdleCompactionNotice
          text={
            "Compacted while idle, before the prompt cache expired. To turn it off, set Claude idle compaction to Off in Settings > Agent orchestration."
          }
        />
      </section>
    </main>
  </MantineProvider>,
);
