import { describe, expect, it } from "vitest";
import {
  ApiError,
  type ApiPostPath,
  type PostBody,
  type PostResult,
} from "../api";
import {
  localSidebarBackend,
  type SidebarBackend,
} from "../components/sidebar/services";
import { itemGroup } from "../components/sidebar/catalog";
import { mergeSidebar, type SidebarSource } from "./mergedSidebar";
import {
  createMergedSidebarServices,
  sidebarOwner,
  sidebarProject,
  sidebarWireRequest,
} from "./sidebarServices";

function fixture() {
  const source = (id: string): SidebarSource => ({
    id,
    online: true,
    sidebar: {
      stateDir: "/same/state",
      projects: [
        {
          id: id === "local" ? "project-id" : "remote-id",
          path: `/${id}`,
          name: id,
          homeServerId: id === "local" ? "home-backend" : "remote",
          ...(id === "remote"
            ? {
                projectAliases: [
                  {
                    serverId: "home-backend",
                    projectId: "project-id",
                    name: "Home",
                  },
                ],
              }
            : {}),
          folders: [{ id: "folder", name: id }],
          organizationRevision: id === "local" ? 3 : 9,
          peerTeamsRevision: id === "local" ? 5 : 8,
        },
      ],
      threads: [
        {
          id: "same",
          name: id,
          cwd: `/${id}`,
          source: "managed",
          isLead: true,
          projectFolder: "folder",
        },
        {
          id: "second",
          name: id,
          cwd: `/${id}`,
          source: "managed",
          isLead: true,
        },
      ],
      rooms: [],
      peerTeams: [
        {
          id: "team",
          name: id,
          projectPath: `/${id}`,
          members: ["same", "second"],
        },
      ],
      peerTeamsVersion: 1,
      projectOrganizationVersion: 1,
      sidebarOrder: {
        revision: 2,
        groups: {
          projects: [`/${id}`],
          [itemGroup(`/${id}`)]: ["same", "second", "unknown"],
        },
      },
      savedOrder: {},
      compact: {},
      collapsed: {},
      indicators: [],
      unread: [],
    },
  });
  const sources = [source("local"), source("remote")];
  return { sources, model: mergeSidebar(sources, 10, true) };
}
function backend(ownerId: string) {
  const values = new Map<string, string>();
  const requests: { path: string; body: unknown }[] = [];
  let failure: Error | undefined;
  let response: unknown = { revision: 3, groups: {} };
  const storage = {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
    removeItem: (key: string) => values.delete(key),
  };
  const api: SidebarBackend = {
    ...localSidebarBackend,
    ownerId,
    storage,
    saved<T>(key: string, fallback: T): T {
      const value = storage.getItem(key);
      return value === null ? fallback : JSON.parse(value);
    },
    save: (key, value) => {
      storage.setItem(key, JSON.stringify(value));
    },
    async post<Path extends ApiPostPath>(
      path: Path,
      body: PostBody<Path>,
    ): Promise<PostResult<Path>> {
      requests.push({ path, body });
      if (failure) throw failure;
      return response as PostResult<Path>;
    },
  };
  return {
    api,
    values,
    requests,
    fail: (value?: Error) => {
      failure = value;
    },
    response: (value: unknown) => {
      response = value;
    },
  };
}

describe("owner sidebar services", () => {
  it("resolves remote folder and team branches to their own source revisions", () => {
    const { model } = fixture();
    const path = model.projectKey("local", "/local");
    const folder = model.folderKey("remote", "/remote", "folder");
    const team = model.teamKey("remote", "team");
    expect(sidebarOwner(model, { kind: "project", path, folder })).toBe(
      "remote",
    );
    expect(sidebarOwner(model, { kind: "project", path, team })).toBe("remote");
    const services = createMergedSidebarServices(
      model,
      new Map([
        ["local", backend("local").api],
        ["remote", backend("remote").api],
      ]),
      () => true,
      { local: "LOC", remote: "REM" },
    );
    expect(services.displayPath(path)).toBe("/local");
    expect(services.displayPath(path, { kind: "project", path, folder })).toBe(
      "/remote",
    );
    const project = sidebarProject(model, "remote", path);
    expect(project.organizationRevision).toBe(9);
    expect(project.peerTeamsRevision).toBe(8);
    expect(project.folders?.map((row) => row.id)).toEqual([folder]);
    expect(
      sidebarWireRequest(model, "remote", "/api/projects", {
        action: "rename_folder",
        path,
        folder_id: folder,
        expected_revision: 3,
        name: "New",
      }),
    ).toEqual({
      action: "rename_folder",
      path: "/remote",
      folder_id: "folder",
      expected_revision: 9,
      name: "New",
    });
  });

  it("decodes chat, folder, team, shared and conversion requests without cross-owner IDs", () => {
    const { model } = fixture();
    const path = model.projectKey("remote", "/remote");
    const id = model.chatKey("remote", "same");
    const folder = model.folderKey("remote", "/remote", "folder");
    expect(
      sidebarWireRequest(model, "remote", "/api/organization", {
        id,
        project_path: path,
        project_folder: folder,
        expected_folder: folder,
        expected_revision: 4,
      }),
    ).toEqual({
      id: "same",
      project_path: "/remote",
      project_folder: "folder",
      expected_folder: "folder",
      expected_revision: 4,
    });
    for (const action of ["save", "delete", "radio", "move", "convert"]) {
      const wire = sidebarWireRequest(model, "remote", "/api/peer-teams", {
        action,
        path,
        team_id: model.teamKey("remote", "team"),
        member: id,
        target: model.chatKey("remote", "second"),
        members: [id],
        expected_revision: 5,
      });
      expect(wire).toMatchObject({
        path: "/remote",
        team_id: "team",
        member: "same",
        target: "second",
        members: ["same"],
        expected_revision: 8,
      });
    }
    expect(() =>
      sidebarWireRequest(model, "local", "/api/organization", {
        id: model.chatKey("local", "same"),
        project_path: path,
        project_folder: folder,
      }),
    ).toThrow("unavailable on this chat's server");
    expect(() =>
      sidebarWireRequest(model, "remote", "/api/peer-teams", {
        action: "save",
        path,
        members: [model.chatKey("local", "same")],
      }),
    ).toThrow("different servers");
  });

  it("remaps folder responses for the existing MoveChatForm validator", async () => {
    const { model } = fixture();
    const a = backend("local");
    const b = backend("remote");
    b.response({ id: "same", projectFolder: "folder" });
    const services = createMergedSidebarServices(
      model,
      new Map([
        ["local", a.api],
        ["remote", b.api],
      ]),
      () => true,
      { local: "LOC", remote: "REM" },
    );
    const id = model.chatKey("remote", "same");
    const result = await services
      .owner({ kind: "chat", id })
      .post("/api/organization", { id, pinned: true });
    expect(result.projectFolder).toBe(
      model.folderKey("remote", "/remote", "folder"),
    );
    expect(a.requests).toEqual([]);
    expect(b.requests[0].body).toEqual({ id: "same", pinned: true });
  });

  it("keeps the exact source revision and IDs after a lost response and a new snapshot", async () => {
    const { model, sources } = fixture();
    const a = backend("local");
    const b = backend("remote");
    const raw = new Map([
      ["local", a.api],
      ["remote", b.api],
    ]);
    const path = model.projectKey("remote", "/remote");
    const target = {
      kind: "project" as const,
      path,
      folder: model.folderKey("remote", "/remote", "folder"),
    };
    const body: PostBody<"/api/projects"> = {
      action: "rename_folder",
      path,
      folder_id: target.folder,
      expected_revision: 9,
      name: "Draft",
      request_id: "exact-request",
    };
    b.fail(new Error("Lost response"));
    await expect(
      createMergedSidebarServices(model, raw, () => true, {})
        .owner(target)
        .post("/api/projects", body),
    ).rejects.toThrow("Lost response");
    sources[1].sidebar.projects[0].organizationRevision = 20;
    const next = createMergedSidebarServices(
      mergeSidebar(sources, 11, true),
      raw,
      () => true,
      {},
    );
    b.fail();
    await next.owner(target).post("/api/projects", body);
    expect(b.requests[1].body).toEqual(b.requests[0].body);
    expect(b.requests[1].body).toMatchObject({
      path: "/remote",
      folder_id: "folder",
      expected_revision: 9,
    });
    expect(b.values.has("studio-sidebar-wire:exact-request")).toBe(false);
    expect(a.requests).toEqual([]);
  });

  it("returns merged order groups and uses the owner's native revision", async () => {
    const { model } = fixture();
    const a = backend("local");
    const b = backend("remote");
    const services = createMergedSidebarServices(
      model,
      new Map([
        ["local", a.api],
        ["remote", b.api],
      ]),
      () => true,
      {},
    );
    const group = itemGroup(model.projectKey("local", "/local"));
    const order = {
      ...model.order,
      [group]: [...model.order[group]].reverse(),
    };
    const result = await services
      .owner({ kind: "order", group })
      .post("/api/projects", {
        action: "reorder",
        groups: order,
        expected_revision: 10,
        request_id: "reorder",
      });
    expect(a.requests[0].body).toMatchObject({ expected_revision: 2 });
    expect(result).toMatchObject({ revision: 11 });
    expect("groups" in result && result.groups?.projects).toContain(
      model.projectKey("remote", "/remote"),
    );
    expect(b.requests).toEqual([]);
  });

  it("refuses offline posts and unavailable canonical metadata without another owner", async () => {
    const { sources } = fixture();
    const model = mergeSidebar([sources[1]], 1, true);
    const b = backend("remote");
    const services = createMergedSidebarServices(
      model,
      new Map([["remote", b.api]]),
      () => false,
      {},
    );
    const id = model.chatKey("remote", "same");
    expect(services.available({ kind: "chat", id })).toBe(false);
    await expect(
      services
        .owner({ kind: "chat", id })
        .post("/api/organization", { id, pinned: true }),
    ).rejects.toThrow("offline");
    const path = model.data.runtime.projects[0].path!;
    await expect(
      services.owner({ kind: "project", path }).post("/api/projects", {
        action: "rename",
        path,
        name: "Draft",
        expected_revision: 0,
      }),
    ).rejects.toThrow("unavailable");
    expect(b.requests).toEqual([]);
  });

  it("clears a definitive API rejection but retains an unknown timeout request", async () => {
    const { model } = fixture();
    const a = backend("local");
    const b = backend("remote");
    const services = createMergedSidebarServices(
      model,
      new Map([
        ["local", a.api],
        ["remote", b.api],
      ]),
      () => true,
      {},
    );
    const id = model.chatKey("remote", "same");
    const body = { id, name: "Draft", request_id: "rename" };
    b.fail(new ApiError("Conflict", 409));
    await expect(
      services.owner({ kind: "chat", id }).post("/api/rename", body),
    ).rejects.toThrow("Conflict");
    expect(b.values.has("studio-sidebar-wire:rename")).toBe(false);
    b.fail(new ApiError("Unknown", 408));
    await expect(
      services.owner({ kind: "chat", id }).post("/api/rename", body),
    ).rejects.toThrow("Unknown");
    expect(b.values.has("studio-sidebar-wire:rename")).toBe(true);
  });
});
