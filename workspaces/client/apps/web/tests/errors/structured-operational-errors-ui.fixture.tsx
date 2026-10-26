import { desktopFixture } from "../files/desktop.fixture.ts";
import type { JsonInput, Agent } from "../../src/types.ts";
import { fixtureSnapshot } from "../fixtures/snapshot.ts";
declare global {
  interface Window {
    structuredOperationalErrorsUiReadRecordings: () => ReturnType<
      typeof listRecordings
    >;
    structuredOperationalErrorsUiCrashes: string[];
    structuredOperationalErrorsUiRenderCase: (
      which: string,
      value: JsonInput,
    ) => Promise<void>;
    structuredOperationalErrorsUiTranscriptionCalls: number;
  }
}
import { required } from "../teams/helpers.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Requests from "../../src/components/questions/Requests.tsx";
import UserMessages from "../../src/components/shell/messages/UserMessages.tsx";
import BackgroundTasks from "../../src/components/shell/BackgroundTasks.tsx";
import CapacityRetry from "../../src/components/CapacityRetry.tsx";
import SafetyBuffering from "../../src/components/conversation/transcript/SafetyBuffering.tsx";
import { Dictation } from "../../src/components/Dictation.tsx";
import { saveRecording, listRecordings } from "../../src/dictation/storage.ts";
window.structuredOperationalErrorsUiReadRecordings = () =>
  listRecordings("lead");
const root = createRoot(required(document.getElementById("root")));
const agent: Agent = {
  id: "lead",
  rootId: "lead",
  name: "Lead",
  epoch: 0,
  accountKey: "default",
  turnId: "turn",
  inFlight: true,
};
const snapshot = fixtureSnapshot([agent]);
const noop = () => {};
const refresh = async () => {};
let renderId = 0;
class Boundary extends React.Component<React.PropsWithChildren> {
  state = { error: "" };
  static getDerivedStateFromError(error: Error) {
    return { error: error.message };
  }
  componentDidCatch(error: Error) {
    window.structuredOperationalErrorsUiCrashes.push(error.message);
  }
  render() {
    return this.state.error
      ? React.createElement("p", { "data-crash": true }, this.state.error)
      : this.props.children;
  }
}
window.structuredOperationalErrorsUiRenderCase = async (which, value) => {
  let component;
  if (which === "request") {
    const request = {
      id: "approval",
      agent: "lead",
      method: "item/commandExecution/requestApproval",
      params: { reason: "Structured operation failed" },
    };
    component = React.createElement(Requests, {
      requests: [request],
      allRequests: [request],
      scope: "fixture",
      agents: [agent],
      refresh,
      notify: noop,
    });
  }
  if (which === "message")
    component = React.createElement(UserMessages, {
      data: {
        ...snapshot,
        runtime: {
          ...snapshot.runtime,
          complaints: [
            {
              id: "message",
              leadId: "lead",
              author: "lead",
              authorName: "Lead",
              recipient: "user",
              title: "User check",
              created: 1,
              version: 1,
            },
          ],
        },
      },
      refresh,
      notify: noop,
    });
  if (which === "background")
    component = React.createElement(BackgroundTasks, {
      opened: true,
      close: noop,
      data: {
        ...snapshot,
        runtime: {
          ...snapshot.runtime,
          tasks: [
            {
              id: "tool",
              agent: "lead",
              kind: "command",
              name: "commandExecution",
              command: "fixture command",
              status: "running",
              created: 1,
              // @ts-expect-error error deliberately contains a structured legacy diagnostic.
              error: value,
              // @ts-expect-error stdinError deliberately contains a structured legacy diagnostic.
              stdinError: value,
            },
          ],
        },
      },
      leadId: "lead",
      openAgent: noop,
      refresh,
      notify: noop,
    });
  if (which === "capacity")
    component = React.createElement(CapacityRetry, {
      agentId: "lead",
      retry: {
        id: "retry",
        status: "scheduled",
        dueAt: Date.now() / 1000 + 100,
        // @ts-expect-error reason deliberately contains a structured legacy diagnostic.
        reason: value,
      },
    });
  if (which === "safety")
    component = React.createElement(SafetyBuffering, {
      agent: {
        ...agent,
        nativeSafetyRetry: {
          epoch: 0,
          accountKey: "default",
          turnId: "turn",
          stage: "failed",
          // @ts-expect-error error deliberately contains a structured legacy diagnostic.
          error: value,
        },
      },
    });
  if (which === "dictation") {
    window.codexDesktop = desktopFixture();
    await saveRecording({
      id: "recording",
      chatId: "lead",
      state: "ready",
      sampleRate: 16000,
      samples: 4,
      created: Date.now(),
      // @ts-expect-error error deliberately contains a structured legacy diagnostic.
      error: value,
    });
    component = React.createElement(Dictation, {
      chatId: "lead",
      onInsert: noop,
    });
  }
  root.render(
    React.createElement(
      MantineProvider,
      null,
      React.createElement(Boundary, { key: ++renderId }, component),
    ),
  );
};
window.structuredOperationalErrorsUiCrashes = [];
