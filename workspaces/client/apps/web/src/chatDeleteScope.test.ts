import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import {
  agentDeleteScope,
  beginConversationDelete,
  claimConversationDeletePost,
  confirmedConversationDeleteIds,
  conversationDeleteReadback,
  definiteDeleteRejection,
  withoutAgentDeleteScope,
} from "./chatDeleteScope";
import type { Agent } from "./types";

const agent = (id: string, parentId: string | null = null) =>
  ({ id, parentId }) as Agent;

describe("agent delete read scope", () => {
  it("includes nested descendants and leaves other chats readable", () => {
    const agents = [
      agent("lead"),
      agent("worker", "lead"),
      agent("nested-worker", "worker"),
      agent("other-lead"),
      agent("other-worker", "other-lead"),
    ];
    const scope = agentDeleteScope(agents, "lead");

    expect([...scope]).toEqual(["lead", "worker", "nested-worker"]);
    expect(withoutAgentDeleteScope(agents, scope).map(({ id }) => id)).toEqual([
      "other-lead",
      "other-worker",
    ]);
  });

  it("suspends descendant readers synchronously before the POST can be claimed", () => {
    const events: string[] = [];
    const pending = beginConversationDelete(
      "lead",
      [agent("lead"), agent("worker", "lead")],
      "/workspace",
      "workspace-id",
      (scope) => events.push(`suspend:${[...scope].join(",")}`),
    );
    events.push("post-effect");
    expect(events).toEqual(["suspend:lead,worker", "post-effect"]);
    expect(pending.phase).toBe("submitting");
  });

  it("claims one POST across effect replay and does not claim later phases", () => {
    const pending = {
      targetId: "lead",
      scope: new Set(["lead", "worker"]),
      stateDir: "/workspace",
      workspaceId: "workspace-id",
      phase: "submitting" as const,
    };
    const started = new WeakSet<typeof pending>();

    expect(claimConversationDeletePost(pending, started)).toBe(true);
    expect(claimConversationDeletePost(pending, started)).toBe(false);
    expect(
      claimConversationDeletePost({ ...pending, phase: "readback" }, started),
    ).toBe(false);
  });

  it("restores readers only for definite rejection responses", () => {
    expect(
      definiteDeleteRejection(new ApiError({ code: "not_found" }, 404)),
    ).toBe(true);
    expect(
      definiteDeleteRejection(new ApiError({ code: "request_pending" }, 409)),
    ).toBe(false);
    expect(
      definiteDeleteRejection(new ApiError({ code: "outcome_unknown" }, 409)),
    ).toBe(false);
    expect(
      definiteDeleteRejection(new ApiError({ code: "timeout" }, 408)),
    ).toBe(false);
    expect(definiteDeleteRejection(new TypeError("response lost"))).toBe(false);
  });

  it("confirms only from an available projection with the entire scope gone", () => {
    const scope = new Set(["lead", "worker"]);
    const expected = { stateDir: "/workspace", workspaceId: "workspace-id" };

    expect(
      conversationDeleteReadback(
        null,
        scope,
        expected,
        "/workspace",
        "workspace-id",
      ),
    ).toBe("unavailable");
    expect(
      conversationDeleteReadback(
        [agent("lead"), { ...agent("worker"), deletedAt: 1 }],
        scope,
        expected,
        "/workspace",
        "workspace-id",
      ),
    ).toBe("still-present");
    expect(
      conversationDeleteReadback(
        [
          { ...agent("lead"), deletedAt: 1 },
          { ...agent("worker"), deletedAt: 1 },
        ],
        scope,
        expected,
        "/workspace",
        "workspace-id",
      ),
    ).toBe("confirmed");
    expect(
      conversationDeleteReadback(
        [],
        scope,
        expected,
        "/workspace",
        "workspace-id",
      ),
    ).toBe("confirmed");
    expect(
      conversationDeleteReadback([], scope, expected, "/other", "workspace-id"),
    ).toBe("unavailable");
    expect(
      conversationDeleteReadback(
        [],
        scope,
        expected,
        "/workspace",
        "other-workspace",
      ),
    ).toBe("unavailable");
  });

  it("accepts only a complete delete response for the requested scope", () => {
    const scope = new Set(["lead", "worker"]);
    expect(
      confirmedConversationDeleteIds(["lead", "worker"], "lead", scope),
    ).toEqual(["lead", "worker"]);
    expect(confirmedConversationDeleteIds([], "lead", scope)).toBeNull();
    expect(confirmedConversationDeleteIds(["lead"], "lead", scope)).toBeNull();
    expect(
      confirmedConversationDeleteIds(["worker"], "lead", scope),
    ).toBeNull();
    expect(
      confirmedConversationDeleteIds(["lead", 4], "lead", scope),
    ).toBeNull();
    expect(
      confirmedConversationDeleteIds(["lead", "unknown"], "lead", scope),
    ).toBeNull();
    expect(confirmedConversationDeleteIds(null, "lead", scope)).toBeNull();
  });
});
