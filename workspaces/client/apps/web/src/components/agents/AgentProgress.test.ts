import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import {
  AgentProgress,
  progressElapsed,
  commandWaitLabel,
} from "./AgentProgress";
import { workerState } from "./WorkerOverview";

describe("agent-list progress", () => {
  it("renders all command stages, elapsed times and last output without a chat", () => {
    vi.spyOn(Date, "now").mockReturnValue(700_000);
    const html = renderToStaticMarkup(
      createElement(AgentProgress, {
        agent: {
          commandProgress: [
            {
              monitorId: "gate",
              stage: "just test-fast",
              startedAt: 60,
              lastOutputLine: "Rust tests passed",
            },
            {
              monitorId: "browser",
              stage: "Browser checks",
              startedAt: 690,
              lastOutputLine: "Opening fixture",
            },
          ],
        },
      }),
    );
    expect(
      commandWaitLabel({
        commandProgress: [
          {
            monitorId: "m",
            stage: "Gate checks",
            startedAt: 60,
            lastOutputLine: "",
          },
        ],
      }),
    ).toBe("Waiting on Gate checks");
    expect(html).toContain("just test-fast");
    expect(html).toContain("10m 40s");
    expect(html).toContain("Rust tests passed");
    expect(html).toContain("Browser checks");
    expect(html).toContain("10s");
    vi.restoreAllMocks();
  });
  it("shows a typed stall and groups it separately from a finished worker", () => {
    const stall = { taskId: "task-14", since: 10 };
    const html = renderToStaticMarkup(
      createElement(AgentProgress, { agent: { stall } }),
    );
    expect(html).toContain("Stalled · task task-14 unsubmitted");
    expect(
      workerState({ id: "worker", status: "completed", stall }, new Set()),
    ).toBe("stalled");
    expect(workerState({ id: "worker", status: "completed" }, new Set())).toBe(
      "completed",
    );
    expect(
      renderToStaticMarkup(createElement(AgentProgress, { agent: {} })),
    ).toBe("");
    expect(progressElapsed(100, 90)).toBe("0s");
  });
});
