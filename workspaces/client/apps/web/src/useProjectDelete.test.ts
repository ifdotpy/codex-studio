import { beforeEach, expect, it, vi } from "vitest";
import type { SidebarServices } from "./components/sidebar/services";
import type { Snapshot } from "./types";
import { ApiError } from "./api";

// A deterministic render/effect harness, including effect replay, without a DOM dependency.
const hooks = vi.hoisted(() => ({
  slots: [] as unknown[],
  cursor: 0,
  effects: [] as (() => void)[],
  dirty: false,
}));
vi.mock("react", () => {
  const effect = (run: () => void, deps: unknown[]) => {
    const index = hooks.cursor++;
    const prior = hooks.slots[index] as unknown[] | undefined;
    if (!prior || deps.some((value, i) => !Object.is(value, prior[i])))
      hooks.effects.push(run);
    hooks.slots[index] = deps;
  };
  return {
    useRef: (value: unknown) => {
      const index = hooks.cursor++;
      return (hooks.slots[index] ||= { current: value });
    },
    useState: (value: unknown) => {
      const index = hooks.cursor++;
      if (!(index in hooks.slots))
        hooks.slots[index] = typeof value === "function" ? value() : value;
      return [
        hooks.slots[index],
        (next: unknown) => {
          hooks.slots[index] =
            typeof next === "function" ? next(hooks.slots[index]) : next;
          hooks.dirty = true;
        },
      ];
    },
    useEffect: effect,
    useLayoutEffect: effect,
  };
});
import { useProjectDelete } from "./useProjectDelete";

beforeEach(() => {
  hooks.slots = [];
  hooks.cursor = 0;
  hooks.effects = [];
  hooks.dirty = false;
});
const project = {
  id: "project",
  path: "/project",
  name: "Example",
  homeServerId: "home",
};
const data = {
  stateDir: "fixture",
  entityWorkspaceId: "workspace",
  entitiesComplete: true,
  threads: [
    {
      id: "lead",
      cwd: "/project",
      projectId: "project",
      projectServerId: "home",
    },
  ],
  runtime: {
    projects: [project],
    projectSequences: { project: 12 },
    rooms: [],
  },
} as unknown as Snapshot;

function fixture() {
  const values = new Map<string, string>();
  const storage = {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
    removeItem: (key: string) => {
      values.delete(key);
    },
  };
  const post = vi.fn().mockImplementation(async (_path, body) => ({
    id: body.id,
    request_id: body.request_id,
    outcome: "deleted",
    deletedAgentIds: ["lead"],
    deletedRoomIds: [],
  }));
  const backend = { post, storage };
  const owner = vi.fn(() => backend);
  const options = {
    data,
    workspaceId: "workspace",
    services: { owner, cache: backend } as unknown as SidebarServices,
    onSuspend: vi.fn(),
    onResume: vi.fn(),
    onFinish: vi.fn(),
    notify: vi.fn(),
  };
  let result: ReturnType<typeof useProjectDelete>;
  const render = () => {
    hooks.cursor = 0;
    hooks.dirty = false;
    result = useProjectDelete(options);
    const effects = hooks.effects.splice(0);
    for (const effect of effects) effect();
    return result;
  };
  const settle = async () => {
    for (let i = 0; i < 5; i++) {
      await Promise.resolve();
      if (hooks.dirty) render();
    }
    return result;
  };
  return { options, render, settle, post, owner, storage, values };
}

it("cancel posts nothing; repeated confirmation and effect replay send once to services.owner", async () => {
  const f = fixture();
  f.render().open("/project");
  f.render().dialog.onCancel();
  expect(f.render().dialog.name).toBeNull();
  expect(f.post).not.toHaveBeenCalled();
  f.render().open("project");
  const confirming = f.render();
  confirming.dialog.onConfirm();
  confirming.dialog.onConfirm();
  expect(f.options.onSuspend).toHaveBeenCalledOnce();
  expect(f.post).not.toHaveBeenCalled();
  f.render();
  await f.settle();
  expect(f.owner).toHaveBeenCalledWith({ kind: "project", path: "/project" });
  expect(f.post).toHaveBeenCalledOnce();
  expect(f.post.mock.calls[0]?.[1]).toMatchObject({
    id: "project",
    expected_project_seq: 12,
    expected_workspace_id: "workspace",
  });
  expect(f.options.onFinish).toHaveBeenCalledWith(["lead"]);
});

it("keeps unknown visible across SSE retirement and recovers only the saved request", async () => {
  const f = fixture();
  f.post.mockRejectedValueOnce(new TypeError("response lost"));
  f.render().open("project");
  f.render().dialog.onConfirm();
  f.render();
  await f.settle();
  expect(f.render().dialog.phase).toBe("unknown");
  expect(f.options.onFinish).not.toHaveBeenCalled();
  const request = f.post.mock.calls[0]?.[1];
  f.options.data = {
    ...data,
    threads: [],
    runtime: { ...data.runtime, projects: [] },
  };
  const retired = f.render();
  expect(f.options.onFinish).toHaveBeenCalledWith(["lead"]);
  expect(retired.dialog.phase).toBe("unknown");
  retired.dialog.onCancel();
  expect(f.render().dialog.phase).toBe("unknown");
  expect(f.post).toHaveBeenCalledOnce();
  retired.dialog.onRecover();
  retired.dialog.onRecover();
  f.render();
  await f.settle();
  expect(f.post).toHaveBeenCalledTimes(2);
  expect(f.post.mock.calls[1]?.[1]).toEqual(request);
  expect(f.render().dialog.name).toBeNull();
});

it("restores a saved intention after remount without automatically replaying HTTP", async () => {
  const f = fixture();
  f.post.mockRejectedValueOnce(new TypeError("response lost"));
  f.render().open("project");
  f.render().dialog.onConfirm();
  f.render();
  await f.settle();
  hooks.slots = [];
  hooks.cursor = 0;
  f.render();
  const restored = await f.settle();
  expect(restored.dialog.phase).toBe("unknown");
  expect(f.post).toHaveBeenCalledOnce();
  restored.dialog.onRecover();
  f.render();
  await f.settle();
  expect(f.post.mock.calls[1]?.[1]).toEqual(f.post.mock.calls[0]?.[1]);
});

it("definite refusal restores readers and shows its reason without a refresh", async () => {
  const f = fixture();
  f.post.mockRejectedValueOnce(
    new ApiError(
      { code: "project_remote_scope", error: "Project spans servers" },
      409,
    ),
  );
  f.render().open("project");
  f.render().dialog.onConfirm();
  f.render();
  await f.settle();
  expect(f.render().dialog.phase).toBe("rejected");
  expect(f.render().dialog.error).toContain("Project spans servers");
  expect(f.options.onResume).toHaveBeenCalledWith(new Set(["lead"]));
  expect(f.options.onFinish).not.toHaveBeenCalled();
  expect(f.values.size).toBe(0);
});

it("never combines an old project sequence with another workspace's identity", () => {
  const f = fixture();
  f.options.workspaceId = "different";
  f.render().open("project");
  expect(f.render().dialog.name).toBeNull();
  f.options.workspaceId = "workspace";
  f.render().open("project");
  const confirming = f.render();
  f.options.workspaceId = "different";
  f.render();
  confirming.dialog.onConfirm();
  f.render();
  expect(f.post).not.toHaveBeenCalled();
  expect(f.values.size).toBe(0);
});

it("does not treat partial or foreign-workspace baselines as project retirement", () => {
  const f = fixture();
  f.render();
  f.options.data = {
    ...data,
    entitiesComplete: false,
    threads: [],
    runtime: { ...data.runtime, projects: [] },
  };
  f.render();
  expect(f.options.onFinish).not.toHaveBeenCalled();
  f.options.data = {
    ...f.options.data,
    entitiesComplete: true,
    entityWorkspaceId: "other",
  };
  f.render();
  expect(f.options.onFinish).not.toHaveBeenCalled();
});

it("remembers readers when chat and project tombstones arrive in separate streamed publications", () => {
  const f = fixture();
  f.render();
  f.options.data = { ...data, threads: [] };
  f.render();
  f.options.data = {
    ...f.options.data,
    runtime: { ...data.runtime, projects: [] },
  };
  f.render();
  expect(f.options.onFinish).toHaveBeenCalledWith(["lead"]);
});

it("never applies an old workspace's late deletion response to the current drafts", async () => {
  const f = fixture();
  let complete!: (value: unknown) => void;
  f.post.mockReturnValueOnce(
    new Promise((resolve) => {
      complete = resolve;
    }),
  );
  f.render().open("project");
  f.render().dialog.onConfirm();
  f.render();
  const request = f.post.mock.calls[0]?.[1];
  f.options.workspaceId = "new-workspace";
  f.options.data = { ...data, entityWorkspaceId: "new-workspace" };
  f.render();
  complete({
    id: request.id,
    request_id: request.request_id,
    outcome: "deleted",
    deletedAgentIds: ["lead"],
    deletedRoomIds: [],
  });
  await f.settle();
  expect(f.options.onFinish).not.toHaveBeenCalled();
  expect(f.render().dialog.name).toBeNull();
});

it("retains suppression after HTTP success until the listed entities retire through SSE", async () => {
  const f = fixture();
  f.render().open("project");
  f.render().dialog.onConfirm();
  f.render();
  await f.settle();
  expect(f.options.onFinish).toHaveBeenCalledWith(["lead"]);
  expect(f.options.onResume).not.toHaveBeenCalled();
  expect(f.options.onSuspend).toHaveBeenLastCalledWith(new Set(["lead"]));
  f.options.data = {
    ...data,
    threads: [],
    runtime: { ...data.runtime, projects: [] },
  };
  f.render();
  expect(f.options.onResume).toHaveBeenCalled();
});

it("registry-only project removal preserves surviving chats, rooms, drafts and selection", () => {
  const f = fixture();
  f.render();
  f.options.data = { ...data, runtime: { ...data.runtime, projects: [] } };
  f.render();
  expect(f.options.onFinish).not.toHaveBeenCalled();
  expect(f.options.onResume).not.toHaveBeenCalled();
});

it("not_applied multi-server refusal restores readers and releases the intention", async () => {
  const f = fixture();
  f.post.mockRejectedValueOnce(
    new ApiError(
      {
        code: "project_remote_scope",
        outcome: "not_applied",
        error: "Project spans servers",
      },
      409,
    ),
  );
  f.render().open("project");
  f.render().dialog.onConfirm();
  f.render();
  await f.settle();
  expect(f.render().dialog.phase).toBe("rejected");
  expect(f.render().dialog.error).toContain("Project spans servers");
  expect(f.options.onResume).toHaveBeenCalledWith(new Set(["lead"]));
  expect(f.values.size).toBe(0);
});

it("does not release confirmed IDs from an incomplete projection", async () => {
  const f = fixture();
  let complete!: (value: unknown) => void;
  f.post.mockReturnValueOnce(
    new Promise((resolve) => {
      complete = resolve;
    }),
  );
  f.render().open("project");
  f.render().dialog.onConfirm();
  f.render();
  const request = f.post.mock.calls[0]?.[1];
  f.options.data = { ...data, entitiesComplete: false, threads: [] };
  f.render();
  complete({
    id: request.id,
    request_id: request.request_id,
    outcome: "deleted",
    deletedAgentIds: ["lead"],
    deletedRoomIds: [],
  });
  await f.settle();
  expect(f.options.onResume).not.toHaveBeenCalled();
  expect(f.options.onSuspend).toHaveBeenLastCalledWith(new Set(["lead"]));
});
