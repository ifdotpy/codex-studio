import { describe, expect, it } from "vitest";
import { isVisibleSidebarAgent, projectDisplayName } from "./Sidebar";

describe("sidebar identity fallbacks", () => {
  it("keeps a managed lead visible when its name is missing", () => {
    expect(
      isVisibleSidebarAgent({
        source: "managed",
        isLead: true,
        deletedAt: null,
        sharedRoomId: null,
      }),
    ).toBe(true);
  });

  it("uses a project's path as its display name when its name is missing", () => {
    expect(projectDisplayName({ path: "/work/project", name: null })).toBe(
      "/work/project",
    );
  });
});
