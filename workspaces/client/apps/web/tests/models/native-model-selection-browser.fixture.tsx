declare global {
  interface Window {
    nativeModelFixture: { compactAlias?: string };
  }
}
window.nativeModelFixture = {};
const rootElement = document.getElementById("root");
if (!rootElement) throw new Error("Missing model fixture root");
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider, Stack, Paper, Button } from "@mantine/core";
import "@mantine/core/styles.css";
import { AgentSetupPicker } from "../../src/components/agents/AgentSetupPicker.tsx";
import { ModelField } from "../../src/components/models/ModelField.tsx";
import "../../src/components/agents/execution-settings.css";
function Fixture() {
  const [model, onModel] = useState("sol");
  return (
    <MantineProvider
      forceColorScheme={
        new URLSearchParams(location.search).get("theme") === "dark"
          ? "dark"
          : "light"
      }
    >
      <Paper p="sm">
        <Stack gap="xs">
          <Button id="before">Before models</Button>
          <AgentSetupPicker
            role="orchestrator"
            onRole={() => {}}
            provider="codex"
            providers={[]}
            onProvider={() => {}}
            accounts={[]}
            accountKey=""
            onAccount={() => {}}
            models={[
              {
                value: "astra",
                label: "Astra 6",
                provider: "codex",
                badges: ["default"],
              },
              {
                value: "unavailable",
                label: "Unavailable model",
                provider: "codex",
                badges: [],
                disabledReason: "Unavailable on this account",
              },
              { value: "sol", label: "Sol 6.1", provider: "codex", badges: [] },
              {
                value: "opus",
                label: "Opus 4.6",
                provider: "claude",
                badges: [],
              },
            ]}
            model={model}
            onModel={onModel}
            modelLabel="Model"
            efforts={[]}
            effort=""
            onEffort={() => {}}
            effortLabel="Reasoning"
            disabled={false}
            fastAvailable={false}
            fast={false}
            onFast={() => {}}
            daybreakAvailable={false}
            daybreak={false}
            onDaybreak={() => {}}
          />
          <Button id="after">After models</Button>
          <ModelField
            presentation="select"
            label="Compact model"
            value={model}
            onChange={onModel}
            options={[
              {
                value: "__model_default__",
                label: "Same as main agent",
                provider: "codex",
                badges: [],
              },
              {
                value: "astra",
                label: "Astra 6",
                provider: "codex",
                description: "General coding",
                badges: ["default", "current"],
              },
              {
                value: "unavailable",
                label: "Unavailable model",
                provider: "codex",
                badges: [],
                disabledReason: "Unavailable on this account",
              },
              {
                value: "opus",
                label: "Opus 4.6",
                provider: "claude",
                description: "Careful reasoning",
                badges: [],
              },
            ]}
          />
          <ModelField
            presentation="select"
            label="Resolved Claude model"
            value="claude-opus-5-5"
            onChange={(value) =>
              (window.nativeModelFixture.compactAlias = value)
            }
            options={[
              {
                value: "default",
                resolvedModel: "claude-opus-5-5",
                label: "Opus 5.5",
                provider: "claude",
                badges: ["default"],
                description: "Provider default",
              },
              {
                value: "opus",
                resolvedModel: "claude-opus-5-5",
                label: "Opus 5.5",
                provider: "claude",
                badges: [],
                description: "Available Opus model",
              },
            ]}
          />
        </Stack>
      </Paper>
    </MantineProvider>
  );
}
createRoot(rootElement).render(<Fixture />);
