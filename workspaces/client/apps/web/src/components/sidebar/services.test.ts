import { describe, expect, it, vi } from "vitest";
import { post, saved, save } from "../../api";
import { serverLocalStorage } from "../../servers/storage";
import { writePreferenceEdit } from "../../sync/uiPreferenceStore";
import {
  createSidebarServices,
  localSidebarBackend,
  localSidebarServices,
  sidebarIdentity,
  type SidebarTarget,
  type SidebarBackend,
} from "./services";

describe("sidebar server ownership", () => {
  it("uses the existing API, storage, preference edits and original IDs by default", () => {
    expect(localSidebarBackend.post).toBe(post);
    expect(localSidebarBackend.saved).toBe(saved);
    expect(localSidebarBackend.save).toBe(save);
    expect(localSidebarBackend.storage).toBe(serverLocalStorage);
    expect(localSidebarBackend.editPreference).toBe(writePreferenceEdit);
    for (const target of [
      { kind: "chat", id: "same-chat" },
      { kind: "project", path: "/same/project" },
      { kind: "order", group: "projects" },
    ] satisfies SidebarTarget[]) {
      expect(localSidebarServices.owner(target)).toBe(localSidebarBackend);
    }
  });

  it("separates overlapping IDs and paths without changing request bodies", async () => {
    const leftPost = vi.fn(post);
    const rightPost = vi.fn(post);
    const left: SidebarBackend = {
      ...localSidebarBackend,
      ownerId: "left",
      post: leftPost as unknown as typeof post,
    };
    const right: SidebarBackend = {
      ...localSidebarBackend,
      ownerId: "right",
      post: rightPost as unknown as typeof post,
    };
    leftPost.mockResolvedValue({});
    rightPost.mockResolvedValue({});
    const services = createSidebarServices(left, [left, right], (target) => {
      const identity =
        target.kind === "chat"
          ? target.id
          : target.kind === "project"
            ? target.path
            : target.group;
      return JSON.parse(identity)[0];
    });
    const body = { id: "same-chat", pinned: true };
    for (const backend of [left, right]) {
      for (const target of [
        { kind: "chat", id: sidebarIdentity(backend.ownerId, "same-chat") },
        {
          kind: "project",
          path: sidebarIdentity(backend.ownerId, "/same/project"),
        },
        { kind: "order", group: sidebarIdentity(backend.ownerId, "projects") },
      ] satisfies SidebarTarget[]) {
        expect(services.owner(target)).toBe(backend);
      }
      await services
        .owner({
          kind: "chat",
          id: sidebarIdentity(backend.ownerId, "same-chat"),
        })
        .post("/api/organization", body);
      expect(backend.post).toHaveBeenCalledWith("/api/organization", body);
      expect((backend === left ? leftPost : rightPost).mock.calls[0][1]).toBe(
        body,
      );
    }
    expect(sidebarIdentity("a:b", "c")).not.toBe(sidebarIdentity("a", "b:c"));
  });

  it("keeps the selected backend for a retry when view selection changes", async () => {
    const leftPost = vi.fn(post);
    const rightPost = vi.fn(post);
    const left: SidebarBackend = {
      ...localSidebarBackend,
      ownerId: "left",
      post: leftPost as unknown as typeof post,
    };
    const right: SidebarBackend = {
      ...localSidebarBackend,
      ownerId: "right",
      post: rightPost as unknown as typeof post,
    };
    let selected = "left";
    const services = createSidebarServices(left, [left, right], () => selected);
    const backend = services.owner({ kind: "chat", id: "same-chat" });
    const body = { id: "same-chat", pinned: true };
    leftPost.mockRejectedValueOnce(new TypeError("Response lost"));
    leftPost.mockResolvedValueOnce({});
    await expect(backend.post("/api/organization", body)).rejects.toThrow(
      "Response lost",
    );
    selected = "right";
    await services.byOwner(backend.ownerId).post("/api/organization", body);
    expect(left.post).toHaveBeenCalledTimes(2);
    expect(leftPost.mock.calls[1][1]).toBe(body);
    expect(right.post).not.toHaveBeenCalled();
  });

  it("rejects missing and duplicate owners instead of using the local server", () => {
    expect(() =>
      createSidebarServices(
        localSidebarBackend,
        [localSidebarBackend, localSidebarBackend],
        () => "local",
      ),
    ).toThrow("Duplicate sidebar server identity");
    const services = createSidebarServices(
      localSidebarBackend,
      [localSidebarBackend],
      () => "missing",
    );
    expect(() => services.owner({ kind: "chat", id: "overlap" })).toThrow(
      "The sidebar server is unavailable",
    );
    expect(() => services.byOwner("missing")).toThrow(
      "The sidebar server is unavailable",
    );
  });
});
