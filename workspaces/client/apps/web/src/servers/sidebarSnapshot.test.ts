import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Snapshot } from "../types";
import { navigationSnapshot } from "./navigation";
import { sidebarSnapshot } from "./sidebarSnapshot";

function snapshot(): Snapshot {
  return {
    token: "session-token",
    stateDir: "/state/original",
    threads: [],
    chats: [],
    nodes: [],
    edges: [],
    runtime: {
      agents: [],
      rooms: [],
      tasks: [],
      monitors: [],
      complaints: [],
      requests: [],
      rules: [],
      projects: [],
      peerTeams: [],
      events: [],
      work: [],
    },
  };
}

let records: Map<string, string>;
let getItem: ReturnType<typeof vi.fn>;
let setItem: ReturnType<typeof vi.fn>;
beforeEach(() => {
  records = new Map();
  getItem = vi.fn((key: string) => records.get(key) ?? null);
  setItem = vi.fn();
  vi.stubGlobal("localStorage", { getItem, setItem });
});
afterEach(() => vi.unstubAllGlobals());

describe("server sidebar projection", () => {
  it("preserves organization, tails, workers and radio membership without transcripts or session state", () => {
    const data = snapshot();
    data.threads = [
      {
        id: "lead",
        source: "managed",
        isLead: true,
        name: "Original chat",
        cwd: "/work",
        projectId: "project",
        projectServerId: "home",
        projectFolder: "nested",
        projectFolderRevision: 4,
        pinned: true,
        archived: true,
        status: "completed",
        updated: 8,
        created: 2,
        tail: "Short search text",
        lastAnswer: "Full answer",
        threadId: "native-thread",
        lastCompletedTurn: "turn",
        lastCompletedTurnStatus: "completed",
        readStateSupported: true,
        readState: {
          threadId: "native-thread",
          turnId: "turn",
          read: false,
          revision: 5,
        },
      },
      {
        id: "worker",
        source: "managed",
        isLead: false,
        parentId: "lead",
        rootId: "lead",
      },
      { id: "hidden", source: "managed", deletedAt: 10, sharedRoomId: "room" },
    ];
    data.runtime.projects = [
      {
        id: "project",
        path: "/work",
        name: "Original project",
        folders: [
          { id: "parent", name: "Parent" },
          { id: "nested", name: "Child", parentId: "parent" },
        ],
        organizationRevision: 7,
        peerTeamsRevision: 3,
        peerTeams: [{ id: "team", name: "Peers", members: ["lead", "second"] }],
        homeServerId: "home",
        locationsRevision: 9,
        locations: [
          {
            path: "/remote/work",
            serverId: "remote",
            projectId: "remote-project",
          },
        ],
        projectAliases: [
          {
            serverId: "remote",
            projectId: "remote-project",
            name: "Remote project",
          },
        ],
      },
    ];
    data.runtime.peerTeams = [
      {
        id: "team",
        name: "Peers",
        projectPath: "/work",
        members: ["lead", "second"],
        revision: 3,
      },
    ];
    data.runtime.rooms = [
      {
        id: "room",
        name: "Shared chat",
        members: ["lead", "worker"],
        projectPath: "/work",
        radio: {
          direct: true,
          teamId: "team",
          revision: 2,
          status: "idle",
          active: null,
          error: null,
          speaker: null,
          next: [],
        },
        lastMessage: {
          text: "Private transcript text",
          sender: "lead",
          seq: 1,
          created: 4,
        },
      },
    ];
    data.runtime.peerTeamsVersion = 1;
    data.runtime.projectOrganizationVersion = 2;
    const before = JSON.stringify(data);
    const projected = sidebarSnapshot(data, new Set(["lead"]));
    expect(projected.threads).toEqual(
      data.threads.map(({ lastAnswer: _lastAnswer, ...agent }) => agent),
    );
    expect(projected.projects).toEqual(data.runtime.projects);
    expect(projected.peerTeams).toEqual(data.runtime.peerTeams);
    expect(projected.rooms).toEqual(
      data.runtime.rooms.map(({ lastMessage: _lastMessage, ...room }) => room),
    );
    expect(projected.peerTeamsVersion).toBe(1);
    expect(projected.projectOrganizationVersion).toBe(2);
    expect(projected.unread).toEqual(["lead"]);
    expect(new Map(projected.indicators).get("lead")).toEqual({
      kind: "unread",
      label: "Unread result",
    });
    expect(Object.keys(projected).sort()).toEqual(
      [
        "stateDir",
        "threads",
        "projects",
        "rooms",
        "peerTeams",
        "peerTeamsVersion",
        "projectOrganizationVersion",
        "sidebarOrder",
        "savedOrder",
        "compact",
        "collapsed",
        "indicators",
        "unread",
      ].sort(),
    );
    expect(JSON.stringify(projected)).not.toContain("session-token");
    expect(JSON.stringify(projected)).not.toContain("Full answer");
    expect(JSON.stringify(projected)).not.toContain("Private transcript text");
    expect(JSON.stringify(data)).toBe(before);
    expect(setItem).not.toHaveBeenCalled();
  });

  it("reads the original saved keys and preserves every group, folder and team key", () => {
    const data = snapshot();
    const folderKey = JSON.stringify(["/work", "folder", "nested"]);
    const teamKey = JSON.stringify(["/work", "team", "team"]);
    const itemGroup = JSON.stringify(["items", "/work", "nested"]);
    const legacyGroup = JSON.stringify([
      "chats",
      "/work",
      true,
      false,
      "team",
      "team",
    ]);
    const compact = { "/work": false, "/other": true };
    const collapsed = { "/work": true, [folderKey]: false, [teamKey]: true };
    const groups = {
      projects: ["/work"],
      [itemGroup]: ["lead", "folder:nested"],
      [legacyGroup]: ["second"],
    };
    records.set(
      `codex-project-compact:${data.stateDir}`,
      JSON.stringify(compact),
    );
    records.set(
      `codex-project-tree:${data.stateDir}`,
      JSON.stringify(collapsed),
    );
    records.set(`codex-sidebar-order:${data.stateDir}`, JSON.stringify(groups));
    records.set(
      "codex-project-compact:/state/different",
      JSON.stringify({ "/work": true }),
    );
    const projected = sidebarSnapshot(data);
    expect(projected.compact).toEqual(compact);
    expect(projected.collapsed).toEqual(collapsed);
    expect(projected.savedOrder).toEqual(groups);
    expect(projected.sidebarOrder).toBeUndefined();
    expect(getItem.mock.calls.map(([key]) => key).sort()).toEqual(
      [
        `codex-project-compact:${data.stateDir}`,
        `codex-project-tree:${data.stateDir}`,
        `codex-sidebar-order:${data.stateDir}`,
      ].sort(),
    );
    expect(setItem).not.toHaveBeenCalled();
  });

  it.each<Snapshot["runtime"]["sidebarOrder"]>([
    undefined,
    null,
    { revision: 4, groups: null },
    { revision: 8, groups: {} },
    { revision: 9, groups: { projects: ["/server"] } },
  ])("retains server order support and migration state: %j", (order) => {
    const data = snapshot();
    data.runtime.sidebarOrder = order;
    records.set(
      `codex-sidebar-order:${data.stateDir}`,
      JSON.stringify({ projects: ["/legacy"] }),
    );
    const projected = sidebarSnapshot(data);
    expect(projected.sidebarOrder).toEqual(order);
    expect(projected.savedOrder).toEqual({ projects: ["/legacy"] });
  });

  it("uses empty saved preferences when keys are missing or invalid", () => {
    const data = snapshot();
    records.set(`codex-project-compact:${data.stateDir}`, "invalid");
    records.set(`codex-project-tree:${data.stateDir}`, "null");
    expect(sidebarSnapshot(data)).toMatchObject({
      compact: {},
      collapsed: {},
      savedOrder: {},
    });
  });

  it("adds the real sidebar snapshot to navigation only after data is available", () => {
    const data = snapshot();
    data.threads = [
      {
        id: "lead",
        source: "managed",
        isLead: true,
        cwd: "/work",
        inFlight: true,
      },
    ];
    const navigation = navigationSnapshot(data, "lead", "", new Set(["lead"]));
    expect(navigation.sidebar).toEqual(
      sidebarSnapshot(data, new Set(["lead"])),
    );
    expect(navigation.chats[0]).toMatchObject({
      id: "lead",
      unread: true,
      inFlight: true,
    });
    expect(navigation.sidebar?.indicators).toEqual([
      ["lead", navigation.chats[0].indicator],
    ]);
    expect(navigationSnapshot(null, null, "")).not.toHaveProperty("sidebar");
  });
});
