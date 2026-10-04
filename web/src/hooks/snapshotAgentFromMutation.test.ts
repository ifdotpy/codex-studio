import { describe, expect, it } from "vitest";
import type { PostResult } from "../api";
import { snapshotAgentFromMutation } from "./snapshotAgentFromMutation";

type AgentMutationResponse = PostResult<"/api/leads">;

describe("agent mutation snapshot normalization", () => {
  it("makes an omitted mutation discriminator immediately displayable", () => {
    const response: AgentMutationResponse = {
      id: "created-agent",
      name: "New chat",
      threadId: null,
    };

    expect(snapshotAgentFromMutation(response)).toMatchObject({
      id: "created-agent",
      name: "New chat",
      source: "managed",
      kind: "agent",
      canSend: true,
    });
  });

  it("preserves exact identity and execution settings when kind is null", () => {
    const response: AgentMutationResponse = {
      id: "requested-agent-id",
      name: "Requested chat",
      threadId: null,
      model: "fixture-model",
      effort: "high",
      fastMode: true,
      kind: null,
      canSend: false,
    };

    expect(snapshotAgentFromMutation(response)).toMatchObject({
      id: "requested-agent-id",
      name: "Requested chat",
      kind: "agent",
      model: "fixture-model",
      effort: "high",
      fastMode: true,
      canSend: false,
    });
  });
});
