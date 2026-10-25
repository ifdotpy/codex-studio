import { describe, expect, it, vi } from "vitest";
import { ApiError } from "./api";
import { ProjectDeleteIntent, projectReaderScope } from "./projectDelete";
import type { Agent, Project } from "./types";

const target = { id: "project", name: "Example", path: "/project", seq: 12 };
const body = {
  id: target.id,
  request_id: "intention",
  expected_workspace_id: "workspace",
  expected_project_seq: 12,
};
const reply = {
  id: target.id,
  request_id: body.request_id,
  outcome: "deleted",
  deletedAgentIds: ["lead", "worker"],
  deletedRoomIds: ["room"],
};
const storage = () => {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
    removeItem: (key: string) => {
      values.delete(key);
    },
  };
};

describe("project deletion intention", () => {
  it("locks one POST and keeps the exact request through response loss and explicit recovery", async () => {
    const store = storage();
    const intent = new ProjectDeleteIntent(
      body,
      "Example",
      ["lead", "worker", "room"],
      store,
    );
    const failed = vi.fn().mockRejectedValue(new TypeError("response lost"));
    const first = intent.submit(failed);
    await intent.submit(failed);
    await first;
    expect(failed).toHaveBeenCalledTimes(1);
    expect(intent.phase).toBe("unknown");
    expect(intent.error).toContain("unconfirmed");
    await intent.submit(failed);
    expect(failed).toHaveBeenCalledTimes(1);
    const recovered = ProjectDeleteIntent.restore(store, "workspace")!;
    expect(recovered.phase).toBe("unknown");
    expect(recovered.request).toEqual(body);
    const post = vi.fn().mockResolvedValue(reply);
    recovered.retry();
    await recovered.submit(post);
    expect(post).toHaveBeenCalledWith("/api/projects/delete", body);
    expect(recovered.phase).toBe("deleted");
    expect(ProjectDeleteIntent.restore(store, "workspace")).toBeNull();
  });

  it("does not call a mismatched or malformed response success", async () => {
    for (const response of [
      { ...reply, request_id: "other" },
      { ...reply, id: "other" },
      { ...reply, deletedRoomIds: null },
    ]) {
      const intent = new ProjectDeleteIntent(body, "Example", [], storage());
      await intent.submit(vi.fn().mockResolvedValue(response));
      expect(intent.phase).toBe("unknown");
    }
  });

  it("restores readers only for definite rejection and preserves the backend refusal reason", async () => {
    const store = storage();
    const intent = new ProjectDeleteIntent(body, "Example", [], store);
    await intent.submit(
      vi
        .fn()
        .mockRejectedValue(
          new ApiError(
            { code: "remote_scope", error: "Project spans servers" },
            409,
          ),
        ),
    );
    expect(intent.phase).toBe("rejected");
    expect(intent.error).toContain("Project spans servers");
    expect(ProjectDeleteIntent.restore(store, "workspace")).toBeNull();
  });

  it("retains uncertain server replies and never recovers against a different workspace", async () => {
    const store = storage();
    const intent = new ProjectDeleteIntent(body, "Example", [], store);
    await intent.submit(
      vi.fn().mockRejectedValue(new ApiError({ code: "outcome_unknown" }, 409)),
    );
    expect(intent.phase).toBe("unknown");
    expect(ProjectDeleteIntent.restore(store, "different")).toBeNull();
  });

  it("does not send if the intention cannot be saved", () => {
    const store = storage();
    store.setItem = () => {
      throw new Error("Storage unavailable");
    };
    expect(() => new ProjectDeleteIntent(body, "Example", [], store)).toThrow(
      "Storage unavailable",
    );
  });
});

describe("project reader suspension", () => {
  it("covers bound chats, archived chats, descendants, legacy exact paths and owned rooms, without prefix matching", () => {
    const agents = [
      {
        id: "lead",
        projectId: "project",
        projectServerId: "home",
        cwd: "/other",
      },
      { id: "worker", parentId: "lead", cwd: "/elsewhere" },
      {
        id: "archived",
        projectId: "project",
        projectServerId: "home",
        archived: true,
      },
      { id: "legacy", cwd: "/project" },
      { id: "unrelated", cwd: "/project/subdirectory" },
      {
        id: "different-binding",
        projectId: "another",
        projectServerId: "home",
        cwd: "/project",
      },
    ] as Agent[];
    const project = { ...target, homeServerId: "home" } as Project;
    expect(
      [
        ...projectReaderScope(project, agents, [
          { id: "room", projectPath: "/project" },
        ]),
      ].sort(),
    ).toEqual(["archived", "lead", "legacy", "room", "worker"]);
  });
});
