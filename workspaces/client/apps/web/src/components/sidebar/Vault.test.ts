import { required } from "../componentTestFixtures";
import { describe, expect, it, vi } from "vitest";
import {
  chipWithVault,
  deskLink,
  vaultActions,
  pullText,
  firstProfile,
  pullVault,
  runJob,
  vaultLabel,
  type ProjectVault,
} from "./Vault";

const vault = (fields: Partial<ProjectVault> = {}): ProjectVault => ({
  projectId: "p1",
  state: "ok",
  prefix: "codex-agents",
  site: "https://desk.example.com/",
  ...fields,
});

describe("vault menu", () => {
  it("labels the vault by its writer report", () => {
    const now = 1_791_720_000_000;
    expect(vaultLabel(vault({ lastPush: now / 1000 - 300 }), now)).toBe(
      "Vault: saved 5 min ago",
    );
    expect(vaultLabel(vault({ lastPush: now / 1000 - 3 * 3600 }), now)).toBe(
      "Vault: saved 3 h ago",
    );
    expect(vaultLabel(vault(), now)).toBe("Vault: no state yet");
    expect(vaultLabel(vault({ state: "error", reason: "403" }), now)).toBe(
      "Vault: the last push failed",
    );
    expect(vaultLabel({ projectId: "p1", state: "none" }, now)).toBe(
      "Vault: not set up",
    );
  });

  it("links to the vault page of an https Desk site only", () => {
    expect(deskLink(vault())).toBe("https://desk.example.com/codex-agents/");
    expect(deskLink(vault({ site: "https://desk.example.com" }))).toBe(
      "https://desk.example.com/codex-agents/",
    );
    expect(deskLink(vault({ site: "http://desk.example.com/" }))).toBeNull();
    expect(deskLink(vault({ site: null }))).toBeNull();
    expect(deskLink(vault({ prefix: null }))).toBeNull();
  });

  it.each([
    "/api/vault/setup",
    "/api/vault/aws-login",
    "/api/vault/pull",
  ] as const)(
    "streams %s receipts with zero timer reads and unsubscribes at completion",
    async (url) => {
      vi.useFakeTimers();
      let changed!: (
        version?: import("../../sync/resourceEvents").ResourceVersion,
      ) => void;
      const remove = vi.fn();
      const post = vi
        .fn()
        .mockResolvedValue({ requestId: "r1", state: "running" });
      const resources = {
        watchResourceChanges: vi.fn((_resource, accept) => {
          changed = accept;
          return remove;
        }),
        watchResourceConnection: () => remove,
      };
      const updates = vi.fn();
      try {
        const pending = runJob(
          { post } as never,
          url,
          { requestId: "r1" },
          { resources, onUpdate: updates },
        );
        await vi.advanceTimersByTimeAsync(10_000);
        expect(post).toHaveBeenCalledTimes(1);
        const resource = required(
          required(resources.watchResourceChanges.mock.calls[0])[0],
        );
        changed({ epoch: "e", revision: 2 });
        changed({
          epoch: "e",
          revision: 3,
          progress: {
            resource,
            value: { requestId: "r1", state: "running", progress: "creating" },
          },
        });
        await vi.advanceTimersByTimeAsync(10_000);
        expect(post).toHaveBeenCalledTimes(1);
        changed({
          epoch: "e",
          revision: 4,
          progress: { resource, value: { requestId: "r1", state: "done" } },
        });
        await expect(pending).resolves.toEqual({
          requestId: "r1",
          state: "done",
        });
        expect(remove).toHaveBeenCalledTimes(2);
        expect(updates).toHaveBeenCalledTimes(3);
      } finally {
        vi.useRealTimers();
      }
    },
  );

  it("recovers one lost response using the same logical request", async () => {
    const post = vi
      .fn()
      .mockRejectedValueOnce(new Error("lost response"))
      .mockResolvedValue({
        requestId: "r1",
        state: "done",
        folder: "/Downloads/demo",
        seq: 12,
        files: 340,
      });
    const done = await pullVault({ post } as never, "p1", "/Downloads", {
      requestId: "r1",
    });
    expect(post.mock.calls.map((call) => required(call[1]))).toEqual(
      Array(2).fill({ projectId: "p1", requestId: "r1", to: "/Downloads" }),
    );
    expect(pullText(done)).toBe(
      "Pulled state 12 (340 files) into /Downloads/demo",
    );
  });

  it("stops watching when the view closes without cancelling the server job", async () => {
    const remove = vi.fn();
    const controller = new AbortController();
    const post = vi
      .fn()
      .mockResolvedValue({ requestId: "r1", state: "running" });
    const pending = runJob(
      { post } as never,
      "/api/vault/setup",
      { requestId: "r1" },
      {
        signal: controller.signal,
        resources: {
          watchResourceChanges: () => remove,
          watchResourceConnection: () => remove,
        },
      },
    );
    await Promise.resolve();
    controller.abort();
    await expect(pending).rejects.toThrow("Stopped watching");
    expect(remove).toHaveBeenCalledTimes(2);
    expect(post).toHaveBeenCalledTimes(1);
  });

  it("puts a cloud on the LAYR chip of a project with a vault", () => {
    const chip = {
      summary: { state: "ready" as const, title: "Ready for layr chats" },
      attention: false,
    };
    const now = 1_791_720_000_000;
    expect(chipWithVault(chip, [], now)).toBe(chip);
    expect(
      chipWithVault(chip, [{ vault: { projectId: "p1", state: "none" } }], now),
    ).toBe(chip);
    expect(
      chipWithVault(
        chip,
        [{ vault: vault({ lastPush: now / 1000 - 600 }), server: "REM" }],
        now,
      ),
    ).toEqual({
      summary: {
        state: "ready",
        title: "Ready for layr chats\nREM: Vault: saved 10 min ago",
      },
      attention: false,
      vault: "ok",
    });
    const failed = chipWithVault(
      chip,
      [{ vault: vault({ state: "error", reason: "403" }) }],
      now,
    );
    expect([failed?.vault, failed?.attention]).toEqual(["error", true]);
    expect(chipWithVault(null, [{ vault: vault() }], now)).toBeNull();
  });

  it("offers unlock, Finder and pull only on this Mac", () => {
    const here = { onThisMac: true, desktop: true };
    expect(vaultActions({ projectId: "p1", state: "none" }, here)).toEqual([
      "setup",
    ]);
    expect(
      vaultActions(
        { projectId: "p1", state: "none" },
        { onThisMac: false, desktop: true },
      ),
    ).toEqual(["guide"]);
    expect(vaultActions(vault({ unlocked: false }), here)).toEqual([
      "desk",
      "unlock",
    ]);
    expect(
      vaultActions(vault({ unlocked: true, disk: "/Users/a/Desk/p" }), here),
    ).toEqual(["desk", "finder", "pull"]);
    expect(
      vaultActions(vault({ unlocked: true, disk: "/Users/a/Desk/p" }), {
        onThisMac: true,
        desktop: false,
      }),
    ).toEqual(["desk", "pull"]);
    expect(
      vaultActions(vault({ unlocked: false }), {
        onThisMac: false,
        desktop: true,
      }),
    ).toEqual(["desk"]);
    expect(vaultActions(vault({ site: null }), here)).toEqual([]);
  });

  it("chooses the gateway's AWS profile first", () => {
    const facts = {
      prefix: "demo",
      keys: false,
      site: "https://d/",
      region: "eu-central-1",
      gatewayProfile: "work",
      profiles: [
        { name: "broken", error: "no keys" },
        { name: "home", signedIn: true },
        { name: "work", signedIn: false },
      ],
    };
    expect(firstProfile(facts)).toBe("work");
    expect(firstProfile({ ...facts, gatewayProfile: "gone" })).toBe("home");
    expect(firstProfile({ ...facts, profiles: [] })).toBeNull();
  });
});
