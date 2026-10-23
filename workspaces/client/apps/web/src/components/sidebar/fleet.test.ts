import { describe, expect, it } from "vitest";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import type { Agent } from "../../types";
import {
  chatIndicators,
  type ChatIndicator,
} from "../chat-status/chatStatusModel";
import {
  needsYou,
  projectStateCounts,
  projectStateLabel,
  relativeChatTime,
} from "./fleet";
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
  it("formats relative time without negative or fractional durations", () => {
    expect(relativeChatTime(120, 100)).toBe("now");
    expect(relativeChatTime(0, 179)).toBe("2m");
    expect(relativeChatTime(0, 7200)).toBe("2h");
    expect(relativeChatTime(0, 172800)).toBe("2d");
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
        defaultServerAlias: "MAC",
        indicator: indicators.get("answer"),
        renaming: false,
        name: "",
        organizing: false,
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
  it("renders state, one live activity line, time, unread/status and action accessibility", () => {
    const html = renderRow();
    expect(html).toContain('data-studio-state="answer"');
    expect(html).toContain('data-chat-status="answer"');
    expect(html).toContain("Needs your answer");
    expect(html).toContain("Checking permissions");
    expect(html).not.toContain("Second command");
    expect(html).toContain('<time class="fleet-chat-time"');
    expect(renderRow({ now: 160 })).toContain(">1m</time>");
    expect(renderRow({ now: 220 })).toContain(">2m</time>");
    expect(html).toContain('aria-label="Unpin Chat answer"');
    expect(html).toContain('aria-label="Actions for Chat answer"');
    expect(html).not.toContain("chat-server-line");
  });
  it("keeps nondefault and offline server identity visible", () => {
    expect(renderRow({ serverAlias: "Remote" })).toContain("Remote");
    const offline = renderRow({ serverOffline: true });
    expect(offline).toContain(
      "MAC is unreachable. Showing the last known state.",
    );
    expect(offline).toContain("Offline");
  });
});
