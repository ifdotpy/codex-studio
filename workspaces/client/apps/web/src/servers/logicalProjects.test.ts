import { describe, expect, it, vi } from "vitest";
import * as api from "../api";
import { ApiError } from "../api";
import { logicalProjects } from "./logicalProjects";
import { projectChatCommand } from "./projectChatCommand";
import {
  durableProjectRequest,
  projectLocationOn,
  projectServerChoices,
  readProjectServers,
  saveProjectLocation,
  ProjectReceiptRefused,
} from "./projectLocations";
import type { ServerNavigation } from "./navigation";
const view = (
  projects: ServerNavigation["projects"],
  chats: ServerNavigation["chats"] = [],
): ServerNavigation => ({
  projects,
  chats,
  ready: true,
  alerts: [],
  opened: null,
  error: "",
});
const project = {
  id: "project:logical",
  path: "project:logical",
  name: "attar",
  homeServerId: "mac-id",
  compact: false,
  locations: [
    {
      serverId: "mac-id",
      path: "/Projects/chrompile",
      projectId: "/Projects/chrompile",
    },
    { serverId: "mbp", path: "/Projects/attar", projectId: "/Projects/attar" },
  ],
};
describe("project locations", () => {
  it("groups both registered folders and bound chats under one project", () => {
    const navigation = {
      local: view(
        [project],
        [
          {
            id: "same",
            name: "Local chat",
            path: "/Projects/chrompile",
            archived: false,
            status: "",
            unread: false,
          },
        ],
      ),
      mbp: view(
        [
          {
            id: "/Projects/attar",
            path: "/Projects/attar",
            name: "attar",
            homeServerId: "mbp",
            projectAliases: [
              { serverId: "mac-id", projectId: project.id, name: "attar" },
            ],
          },
        ],
        [
          {
            id: "same",
            name: "Remote chat",
            path: "/Projects/attar",
            projectId: project.id,
            projectServerId: "mac-id",
            archived: false,
            status: "",
            unread: false,
          },
        ],
      ),
    };
    const groups = logicalProjects(navigation);
    expect(groups).toHaveLength(1);
    expect(groups[0].owner).toBe("local");
    expect(groups[0].deleteTarget).toBe(project.id);
    expect(groups[0].compact).toBe(false);
    expect(groups[0].locations?.map((row) => row.serverId)).toEqual([
      "local",
      "mbp",
    ]);
    expect(groups[0].chats.map((row) => row.serverId)).toEqual([
      "local",
      "mbp",
    ]);
  });
  it("keeps different legacy projects separate even when their paths are equal", () => {
    expect(
      logicalProjects({
        local: view([{ path: "/same", name: "Mac" }]),
        mbp: view([{ path: "/same", name: "MBP" }]),
      }),
    ).toHaveLength(2);
  });
  it("finds the actual local server identity and a remote-only location", () => {
    expect(
      projectLocationOn(project, {
        id: "local",
        label: "This computer",
        serverId: "mac-id",
      })?.path,
    ).toBe("/Projects/chrompile");
    expect(
      projectLocationOn(
        { ...project, locations: [project.locations[1]] },
        { id: "local", label: "This computer" },
      ),
    ).toBeUndefined();
  });
  it("disables an offline server before chat selection", () => {
    const choices = projectServerChoices(
      [
        { id: "local", label: "Mac", origin: "http://localhost" },
        { id: "mbp", label: "MBP", origin: "https://mbp.example.ts.net" },
      ],
      { mbp: "offline" },
    );
    expect(choices[0].disabled).toBe(false);
    expect(choices[1].disabled).toBe(true);
  });
  it("keeps an unresolved request identity across dialog reloads", () => {
    const entries = new Map<string, string>();
    const storage = {
      getItem: (key: string) => entries.get(key) || null,
      setItem: (key: string, value: string) => entries.set(key, value),
      removeItem: (key: string) => entries.delete(key),
    };
    const body = { server: "mbp", path: "/Projects/attar" };
    expect(durableProjectRequest(body, "first", storage).id).toBe("first");
    const retried = durableProjectRequest(body, "second", storage);
    expect(retried.id).toBe("first");
    retried.finish();
    expect(durableProjectRequest(body, "next", storage).id).toBe("next");
  });
  it("preserves identities across HTTP refusals and transport uncertainty", () => {
    const entries = new Map<string, string>();
    const storage = {
      getItem: (key: string) => entries.get(key) || null,
      setItem: (key: string, value: string) => entries.set(key, value),
      removeItem: (key: string) => entries.delete(key),
    };
    const body = {
      action: "add_location",
      project: project.id,
      server: "mbp",
      path: "/Projects/attar",
    };
    const refusal = new ApiError("Refused", 403);
    durableProjectRequest(body, "fresh", storage).reject(refusal);
    expect(entries.size).toBe(1);
    durableProjectRequest(body, "unknown", storage).reject(
      new Error("Timeout"),
    );
    const retry = durableProjectRequest(body, "replacement", storage);
    retry.reject(refusal);
    expect(durableProjectRequest(body, "next", storage).id).toBe("fresh");
  });
  it("disables paired but unreachable servers in frame selectors", async () => {
    const get = vi.spyOn(api, "get").mockResolvedValue({
      identity: { serverId: "mac-id", label: "Lumina Mac" },
      servers: [
        {
          id: "mbp",
          label: "MBP",
          status: "paired",
          reachability: "unreachable",
        },
      ],
    } as never);
    try {
      const choices = await readProjectServers();
      expect(choices[0].label).toBe("Lumina Mac");
      expect(choices[1].disabled).toBe(true);
    } finally {
      get.mockRestore();
    }
  });
  it("reads the same location receipt after HTTP refusal and clears a final receipt", async () => {
    const body = {
      action: "add_location" as const,
      project: "project:receipt-test",
      server: "mbp",
      path: "/receipt-test",
      request_id: "original",
    };
    const entries = new Map<string, string>();
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => entries.get(key) || null,
      setItem: (key: string, value: string) => entries.set(key, value),
      removeItem: (key: string) => entries.delete(key),
    });
    const session = vi
      .spyOn(api, "refreshSession")
      .mockResolvedValue({ token: "local-test" } as never);
    const post = vi
      .spyOn(api, "post")
      .mockRejectedValueOnce(new ApiError("Invalid peer reply", 403))
      .mockResolvedValueOnce({ outcome: "applied" } as never)
      .mockResolvedValueOnce({
        outcome: "not_applied",
        error: "Folder is unavailable",
      } as never)
      .mockResolvedValueOnce({ outcome: "applied" } as never);
    try {
      await expect(saveProjectLocation(body)).rejects.toThrow(
        "Invalid peer reply",
      );
      await saveProjectLocation({ ...body, request_id: "new-modal" });
      expect(post.mock.calls[1][1]).toMatchObject({ request_id: "original" });
      await expect(
        saveProjectLocation({ ...body, request_id: "final-refusal" }),
      ).rejects.toBeInstanceOf(ProjectReceiptRefused);
      await saveProjectLocation({ ...body, request_id: "after-final" });
      expect(post.mock.calls[2][1]).toMatchObject({
        request_id: "final-refusal",
      });
      expect(post.mock.calls[3][1]).toMatchObject({
        request_id: "after-final",
      });
    } finally {
      post.mockRestore();
      session.mockRestore();
      vi.unstubAllGlobals();
    }
  });
  it("refuses cross-server commands for unregistered paths or the wrong project owner", () => {
    const choice = {
      target: "mbp",
      path: "/Projects/attar",
      projectId: project.id,
      projectServerId: "mac-id",
    };
    expect(projectChatCommand(view([project]), choice)).toEqual({
      action: "new-chat",
      path: choice.path,
      projectId: choice.projectId,
      projectServerId: "mac-id",
    });
    expect(
      projectChatCommand(view([project]), { ...choice, path: "/other" }),
    ).toBeNull();
    expect(
      projectChatCommand(view([project]), {
        ...choice,
        projectServerId: "other",
      }),
    ).toBeNull();
  });
});

it("only registered logical projects expose a native deletion target", () => {
  const groups = logicalProjects({
    local: view(
      [{ id: "saved", name: "Saved", path: "/saved" }],
      [
        {
          id: "inferred",
          name: "Inferred",
          path: "/inferred",
          archived: false,
          status: "",
          unread: false,
        },
        {
          id: "other",
          name: "Other",
          path: "",
          archived: false,
          status: "",
          unread: false,
        },
      ],
    ),
  });
  expect(groups.find((group) => group.id === "saved")?.deleteTarget).toBe(
    "saved",
  );
  expect(
    groups.find((group) => group.path === "/inferred")?.deleteTarget,
  ).toBeUndefined();
  expect(
    groups.find((group) => group.path === "")?.deleteTarget,
  ).toBeUndefined();
});

it("keeps project mutation tokens and uncertain receipt IDs with the captured owner", async () => {
  const owner = (token: string) => {
    const entries = new Map<string, string>();
    return {
      storage: {
        getItem: (key: string) => entries.get(key) ?? null,
        setItem: (key: string, value: string) => {
          entries.set(key, value);
        },
        removeItem: (key: string) => {
          entries.delete(key);
        },
      },
      refreshSession: vi.fn(async () => ({
        token,
      })) as unknown as typeof api.refreshSession,
      post: vi
        .fn<typeof api.post<"/api/projects">>()
        .mockResolvedValue({ outcome: "applied" } as never),
    };
  };
  const a = owner("owner-a");
  const b = owner("owner-b");
  const body = {
    action: "add_location" as const,
    project: "same",
    server: "peer",
    path: "/same",
    request_id: "a-original",
  };
  a.post.mockRejectedValueOnce(new TypeError("Reply lost"));
  await expect(saveProjectLocation(body, a)).rejects.toThrow("Reply lost");
  await saveProjectLocation({ ...body, request_id: "b-original" }, b);
  await saveProjectLocation({ ...body, request_id: "a-new-attempt" }, a);
  expect(a.post.mock.calls[1]).toEqual([
    "/api/projects",
    expect.objectContaining({ request_id: "a-original" }),
    { sessionToken: "owner-a" },
  ]);
  expect(b.post.mock.calls[0]).toEqual([
    "/api/projects",
    expect.objectContaining({ request_id: "b-original" }),
    { sessionToken: "owner-b" },
  ]);
});
