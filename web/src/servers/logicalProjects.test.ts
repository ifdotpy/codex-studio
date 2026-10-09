import { describe, expect, it } from "vitest";
import { ApiError } from "../api";
import { logicalProjects } from "./logicalProjects";
import { projectChatCommand } from "./projectChatCommand";
import {
  durableProjectRequest,
  projectLocationOn,
  projectServerChoices,
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
        label: "This Mac",
        serverId: "mac-id",
      })?.path,
    ).toBe("/Projects/chrompile");
    expect(
      projectLocationOn(
        { ...project, locations: [project.locations[1]] },
        { id: "local", label: "This Mac" },
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
  it("clears a fresh refusal but preserves a previous uncertain identity", () => {
    const entries = new Map<string, string>();
    const storage = {
      getItem: (key: string) => entries.get(key) || null,
      setItem: (key: string, value: string) => entries.set(key, value),
      removeItem: (key: string) => entries.delete(key),
    };
    const body = { server: "mbp", path: "/Projects/attar" };
    const refusal = new ApiError("Refused", 403);
    durableProjectRequest(body, "fresh", storage).reject(refusal);
    expect(entries.size).toBe(0);
    durableProjectRequest(body, "unknown", storage).reject(
      new Error("Timeout"),
    );
    const retry = durableProjectRequest(body, "replacement", storage);
    retry.reject(refusal);
    expect(durableProjectRequest(body, "next", storage).id).toBe("unknown");
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
