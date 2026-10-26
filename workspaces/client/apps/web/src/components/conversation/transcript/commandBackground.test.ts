import { required } from "../../componentTestFixtures";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { describe, expect, it } from "vitest";
import { ApiError, NetworkTimeoutError } from "../../../api";
import type { Message } from "../../../types";
import Activity, { ToolCard } from "./Activity";
import {
  checkCommandBackground,
  requestCommandBackground,
} from "./commandBackground";

const target = {
  agent: "lead",
  accountKey: "acct",
  epoch: 4,
  connectionId: "connection",
  threadId: "thread",
  turnId: "turn",
};

type Call = { method: "get" | "post"; path: string; value: unknown };

/** A fake server API that records every call and answers from a script. */
function fakeApi(answers: {
  target?: typeof target | null;
  post?: () => unknown;
  receipt?: () => unknown;
}) {
  const calls: Call[] = [];
  const api = {
    get: async (path: string, options: { query: unknown }) => {
      calls.push({ method: "get", path, value: options.query });
      if (path === "/api/processes")
        return {
          processes: [],
          responseTarget:
            answers.target === undefined ? target : answers.target,
        };
      return answers.receipt?.() ?? { found: false, result: null };
    },
    post: async (path: string, body: unknown) => {
      calls.push({ method: "post", path, value: body });
      return answers.post?.();
    },
  };
  return { api: api as never, calls };
}

const posts = (calls: Call[]) => calls.filter((c) => c.method === "post");

describe("requestCommandBackground", () => {
  it("sends one exact request for the item of the current turn", async () => {
    const { api, calls } = fakeApi({
      post: () => ({
        requestId: "r",
        status: "completed",
        processes: [],
        response: { status: "backgrounded" },
      }),
    });
    expect(
      await requestCommandBackground(api, "lead", "turn", "toolu-1"),
    ).toEqual({ kind: "backgrounded" });
    const [post] = posts(calls);
    if (!post) throw new Error("Expected post fixture result");
    expect(post.path).toBe("/api/processes/action");
    expect(post.value).toMatchObject({
      action: "background_command",
      agent: "lead",
      accountKey: "acct",
      epoch: 4,
      responseTarget: target,
      itemId: "toolu-1",
    });
  });

  it("refuses a command from an earlier turn without a write", async () => {
    const { api, calls } = fakeApi({});
    const result = await requestCommandBackground(
      api,
      "lead",
      "old",
      "toolu-1",
    );
    expect(result.kind).toBe("failed");
    expect(posts(calls)).toEqual([]);
    const idle = fakeApi({ target: null });
    expect(
      (await requestCommandBackground(idle.api, "lead", "turn", "toolu-1"))
        .kind,
    ).toBe("failed");
    expect(posts(idle.calls)).toEqual([]);
  });

  it("shows the provider refusal as a failure", async () => {
    const { api } = fakeApi({
      post: () => ({
        requestId: "r",
        status: "completed",
        processes: [],
        response: {
          status: "failed",
          error:
            "This provider cannot move a running command to the background",
        },
      }),
    });
    expect(
      await requestCommandBackground(api, "lead", "turn", "toolu-1"),
    ).toEqual({
      kind: "failed",
      message: "This provider cannot move a running command to the background",
    });
    const rejected = fakeApi({
      post: () => {
        throw new ApiError("The response changed", 400);
      },
    });
    expect(
      (await requestCommandBackground(rejected.api, "lead", "turn", "toolu-1"))
        .kind,
    ).toBe("failed");
  });

  it("keeps a lost reply uncertain and checks it with the same request ID", async () => {
    let attempt = 0;
    const { api, calls } = fakeApi({
      post: () => {
        attempt++;
        if (attempt === 1) throw new NetworkTimeoutError();
        return {
          requestId: "r",
          status: "completed",
          processes: [],
          response: { status: "backgrounded" },
        };
      },
    });
    const first = await requestCommandBackground(
      api,
      "lead",
      "turn",
      "toolu-1",
    );
    expect(first.kind).toBe("uncertain");
    if (first.kind !== "uncertain") return;
    // No receipt: the same request is sent again, so the server applies it once.
    expect(await checkCommandBackground(api, first.request)).toEqual({
      kind: "backgrounded",
    });
    const sent = posts(calls).map(
      (c) => (c.value as { requestId: string }).requestId,
    );
    expect(sent).toHaveLength(2);
    expect(required(sent[1])).toBe(required(sent[0]));
  });

  it("reads a saved receipt without a second write", async () => {
    const { api, calls } = fakeApi({
      post: () => {
        throw new NetworkTimeoutError();
      },
      receipt: () => ({
        found: true,
        result: {
          requestId: "r",
          status: "completed",
          processes: [],
          response: { status: "not_running" },
        },
      }),
    });
    const first = await requestCommandBackground(
      api,
      "lead",
      "turn",
      "toolu-1",
    );
    if (first.kind !== "uncertain") throw new Error("expected uncertainty");
    expect(await checkCommandBackground(api, first.request)).toEqual({
      kind: "notRunning",
    });
    expect(posts(calls)).toHaveLength(1);
  });
});

const command = (fields: Record<string, unknown>, toolStatus?: string) =>
  ({
    id: "lead:toolu-1",
    role: "output",
    title: "commandExecution",
    turnId: "turn",
    toolStatus,
    text: JSON.stringify({
      id: "toolu-1",
      type: "commandExecution",
      command: "sleep 600",
      ...fields,
    }),
  }) as Message;

const card = (item: Message) =>
  renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(ToolCard, { item, agentId: "lead" }),
    ),
  );
const group = (items: Message[]) =>
  renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(Activity, { items, agentId: "lead" }),
    ),
  );

describe("Move to background button", () => {
  it("appears only for a running command whose provider offers the control", () => {
    const offered = command(
      { status: "inProgress", canMoveToBackground: true },
      "running",
    );
    expect(card(offered)).toContain("Move to background");
    // Codex items never carry the flag: no control, no fake behavior.
    expect(card(command({ status: "inProgress" }, "running"))).not.toContain(
      "Move to background",
    );
    expect(
      card(
        command(
          { status: "completed", canMoveToBackground: true, exitCode: 0 },
          "completed",
        ),
      ),
    ).not.toContain("Move to background");
  });

  it("marks a moved command as running in the background", () => {
    const markup = card(
      command({ status: "completed", backgroundTaskId: "bash-1" }, "completed"),
    );
    expect(markup).toContain("In background");
    expect(markup).toContain('data-tool-status="background"');
    expect(markup).not.toContain("Move to background");
  });

  it("is offered on a closed group of tool calls", () => {
    const items = [
      command({ status: "completed", exitCode: 0 }, "completed"),
      {
        ...command({ status: "completed", exitCode: 0 }, "completed"),
        id: "b",
      },
      {
        ...command(
          { status: "inProgress", canMoveToBackground: true },
          "running",
        ),
        id: "c",
      },
    ];
    expect(group(items)).toContain("Move to background");
  });
});
