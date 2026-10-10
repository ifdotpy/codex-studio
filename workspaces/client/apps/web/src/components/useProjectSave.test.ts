import { describe, expect, it } from "vitest";
import type { PostBody } from "../api";
import { persistedProjectRequest } from "./useProjectSave";

describe("persistedProjectRequest", () => {
  it("reuses the exact durable request body when a retry supplies a new body", () => {
    const persisted: PostBody<"/api/peer-teams"> = {
      action: "move",
      path: "/work/project",
      member: "source-lead",
      team_id: "team-1",
      expected_revision: 4,
      request_id: "request-1",
    };
    const next: PostBody<"/api/peer-teams"> = {
      ...persisted,
      request_id: "request-2",
      expected_revision: 5,
    };

    expect(persistedProjectRequest(persisted, next)).toBe(persisted);
  });
});
