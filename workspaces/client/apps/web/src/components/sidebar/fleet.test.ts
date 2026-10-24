import { describe, expect, it } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import type { Agent } from "../../types";
import {
  chatIndicators,
  type ChatIndicator,
} from "../chat-status/chatStatusModel";
import { needsYou, projectStateCounts, projectStateLabel } from "./fleet";
import ProjectStateCounts from "./ProjectStateCounts";
import SidebarRow from "../SidebarRow";

const chat = (id: string, cwd = "/work/api") =>
  ({ id, cwd, name: `Chat ${id}` }) as Agent;
const indicators = new Map<string, ChatIndicator>([
  ["working", { kind: "working", label: "Working" }],
  ["answer", { kind: "answer", label: "Needs your answer" }],
  ["error", { kind: "error", label: "Failed" }],
  ["unread", { kind: "unread", label: "Unread result" }],
  ["paused", { kind: "paused", label: "Stopped" }],
]);

describe("fleet presentation", () => {
  it("prioritizes answers over errors across projects, retaining source order within urgency", () => {
    const rows = [
      chat("error"),
      chat("working"),
      chat("answer", "/work/web"),
      chat("unread"),
      chat("paused"),
      chat("none"),
    ];
    expect(needsYou(rows, indicators).map((row) => row.id)).toEqual([
      "answer",
      "error",
    ]);
    expect(rows.map((row) => row.id)).toEqual([
      "error",
      "working",
      "answer",
      "unread",
      "paused",
      "none",
    ]);
    expect(needsYou(rows, new Map())).toEqual([]);
  });
  it("counts only operator states and exposes a meaningful project summary", () => {
    const counts = projectStateCounts(
      [...indicators.keys()].map((id) => chat(id)),
      indicators,
    );
    expect(counts).toEqual({ working: 1, answer: 1, error: 1 });
    expect(projectStateLabel({ working: 2, answer: 1, error: 0 })).toBe(
      "2 working, 1 needs you",
    );
    const html = renderToStaticMarkup(
      createElement(ProjectStateCounts, { counts }),
    );
    expect(html).toContain('aria-label="1 working, 1 needs you, 1 failed"');
    expect(
      renderToStaticMarkup(
        createElement(ProjectStateCounts, {
          counts: { working: 0, answer: 0, error: 0 },
        }),
      ),
    ).toBe("");
  });
  it("uses the supplied indicator even when the agent's saved status differs", () => {
    const row: Agent = { ...chat("answer"), status: "running" };
    expect(needsYou([row], indicators)).toEqual([row]);
    // The operational owner still decides a failed chat is an error.
    const actual = chatIndicators({
      threads: [{ ...chat("failed"), source: "managed", status: "failed" }],
    } as never);
    expect(projectStateCounts([chat("failed")], actual).error).toBe(1);
  });
});

function renderRow(overrides: Partial<Parameters<typeof SidebarRow>[0]> = {}) {
  const noop = () => {};
  return renderToStaticMarkup(
    createElement(
      MantineProvider,
      {},
      createElement(SidebarRow, {
        row: {
          ...chat("answer"),
          updated: 100,
          pinned: true,
          commandProgress: [
            {
              monitorId: "test",
              stage: "Checking permissions",
              startedAt: 50,
              lastOutputLine: "Output details",
            },
            {
              monitorId: "next",
              stage: "Second command",
              startedAt: 60,
              lastOutputLine: "",
            },
          ],
        },
        selected: true,
        serverAlias: "MAC",
        showServer: false,
        indicator: indicators.get("answer"),
        renaming: false,
        organizing: false,
        name: "",
        compact: false,
        markingRead: false,
        bindings: {},
        actions: {
          prepare: noop,
          open: noop,
          pin: noop,
          rename: noop,
          name: noop,
          cancelRename: noop,
          beginRename: noop,
          unread: noop,
          move: noop,
          changeProject: noop,
          archive: noop,
          remove: noop,
        },
        ...overrides,
      }),
    ),
  );
}

describe("real fleet strip", () => {
  it("keeps the title, state, pin and accessible menu without routine metadata", () => {
    const html = renderRow();
    expect(html).toContain('data-studio-state="answer"');
    expect(html).toContain('data-chat-status="answer"');
    expect(html).toContain("Needs your answer");
    expect(html).toContain("Chat answer");
    expect(html).toContain("chat-pin");
    expect(html).toContain('aria-label="Actions for Chat answer"');
    for (const detail of [
      "Checking permissions",
      "Second command",
      "Output details",
      "Elapsed time",
      "fleet-chat-time",
      "fleet-chat-provider",
      "fleet-chat-meta",
      "row-pin-action",
    ])
      expect(html).not.toContain(detail);
  });
  it("preserves the stall attention cue", () => {
    const html = renderRow({
      row: { ...chat("stalled"), stall: { taskId: "task-4", since: 10 } },
    });
    expect(html).toContain('role="status"');
    expect(html).toContain("Waiting for lead");
  });
  it("shows only non-default server identities and retains offline attention", () => {
    const quiet = renderRow({ row: chat("quiet") });
    expect(quiet).not.toContain("fleet-server-badge");
    expect(quiet).not.toContain("fleet-chat-meta");
    const remote = renderRow({ serverAlias: "RMT", showServer: true });
    expect(remote).toContain('aria-label="Server RMT"');
    expect(remote.indexOf('aria-label="Server RMT"')).toBeGreaterThan(
      remote.indexOf("</strong>"),
    );
    const offline = renderRow({ serverOffline: true });
    expect(offline).toContain(
      "MAC is unreachable. Showing the last known state.",
    );
    expect(offline).toContain(">Offline</span>");
    expect(offline).not.toContain(">MAC</span>");
    expect(offline).toContain('data-offline="true"');
  });
});
