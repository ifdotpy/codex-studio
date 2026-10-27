import { required } from "../componentTestFixtures";
import { describe, expect, it, vi } from "vitest";
import {
  chipWithVault,
  deskLink,
  vaultActions,
  pullText,
  firstProfile,
  pullVault,
  loginVault,
  setupVault,
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

type Progress = import("../../sync/resourceEvents").ResourceProgress;
function jobFrames(path: string): {
  running: Progress;
  update: Progress;
  done: Progress;
} {
  if (path === "/api/vault/setup") {
    const resource = {
      kind: "vault-job-progress",
      job: "setup",
      requestId: "r1",
    } as const;
    return {
      running: {
        resource,
        value: { requestId: "r1", state: "running", prefix: "demo" },
      },
      update: {
        resource,
        value: {
          requestId: "r1",
          state: "running",
          prefix: "demo",
          progress: "creating",
        },
      },
      done: {
        resource,
        value: {
          requestId: "r1",
          state: "done",
          prefix: "demo",
          site: "https://desk.example/",
        },
      },
    };
  }
  if (path === "/api/vault/aws-login") {
    const resource = {
      kind: "vault-job-progress",
      job: "login",
      requestId: "r1",
    } as const;
    return {
      running: { resource, value: { requestId: "r1", state: "running" } },
      update: {
        resource,
        value: {
          requestId: "r1",
          state: "running",
          page: "https://device.example/",
          code: "fixture",
        },
      },
      done: { resource, value: { requestId: "r1", state: "done" } },
    };
  }
  const resource = {
    kind: "vault-job-progress",
    job: "pull",
    requestId: "r1",
  } as const;
  return {
    running: {
      resource,
      value: { requestId: "r1", state: "running", folder: "/fixture/demo" },
    },
    update: {
      resource,
      value: {
        requestId: "r1",
        state: "running",
        folder: "/fixture/demo",
        files: 1,
      },
    },
    done: {
      resource,
      value: {
        requestId: "r1",
        state: "done",
        folder: "/fixture/demo",
        seq: 7,
        files: 3,
        bytes: 1024,
      },
    },
  };
}

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
      const frames = jobFrames(url);
      const post = vi.fn().mockResolvedValue(frames.running.value);
      const resources = {
        watchResourceChanges: vi.fn((_resource, accept) => {
          changed = accept;
          return remove;
        }),
        watchResourceConnection: () => remove,
      };
      const updates = vi.fn();
      try {
        const backend = { post } as never;
        const options = { resources, onUpdate: updates };
        const pending =
          url === "/api/vault/aws-login"
            ? loginVault(
                backend,
                { requestId: "r1", profile: "fixture" },
                options,
              )
            : url === "/api/vault/setup"
              ? setupVault(
                  backend,
                  {
                    requestId: "r1",
                    projectId: "p1",
                    prefix: "demo",
                    profile: "fixture",
                    region: "eu-central-1",
                  },
                  options,
                )
              : pullVault(backend, "p1", null, { requestId: "r1", ...options });
        await vi.advanceTimersByTimeAsync(10_000);
        expect(post).toHaveBeenCalledTimes(1);
        expect(
          required(resources.watchResourceChanges.mock.calls[0])[0],
        ).toEqual(frames.running.resource);
        changed({ epoch: "e", revision: 2 });
        changed({
          epoch: "e",
          revision: 3,
          progress: frames.update,
        });
        await vi.advanceTimersByTimeAsync(10_000);
        expect(post).toHaveBeenCalledTimes(1);
        changed({
          epoch: "e",
          revision: 4,
          progress: frames.done,
        });
        await expect(pending).resolves.toEqual(frames.done.value);
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
        bytes: 1024,
      } satisfies import("./Vault").VaultPull);
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

  it("ignores other jobs and reconnects with the original pull request", async () => {
    let changed!: (
      version?: import("../../sync/resourceEvents").ResourceVersion,
    ) => void;
    let connected!: (
      state: import("../../sync/connectionState").ConnectionHealth,
    ) => void;
    const frames = jobFrames("/api/vault/pull");
    const post = vi
      .fn()
      .mockResolvedValueOnce(frames.running.value)
      .mockResolvedValueOnce(frames.done.value);
    const remove = vi.fn();
    const updates = vi.fn();
    const pending = pullVault({ post } as never, "p1", null, {
      requestId: "r1",
      onUpdate: updates,
      resources: {
        watchResourceChanges: (_resource, accept) => {
          changed = accept;
          return remove;
        },
        watchResourceConnection: (accept) => {
          connected = (state) =>
            accept(state, state === "offline" ? "offline" : "open", {});
          return remove;
        },
      },
    });
    await Promise.resolve();
    changed({
      epoch: "e",
      revision: 2,
      progress: jobFrames("/api/vault/aws-login").done,
    });
    changed({
      epoch: "e",
      revision: 3,
      progress: {
        resource: {
          kind: "vault-job-progress",
          job: "pull",
          requestId: "another",
        },
        value: {
          requestId: "another",
          state: "done",
          folder: "/other",
          seq: 1,
          files: 1,
          bytes: 1,
        },
      },
    });
    expect(updates).toHaveBeenCalledTimes(1);
    connected("offline");
    connected("live");
    await expect(pending).resolves.toEqual(frames.done.value);
    expect(post).toHaveBeenCalledTimes(2);
    expect(post.mock.calls[0]).toEqual(post.mock.calls[1]);
    expect(required(post.mock.calls[1])[1]).toEqual({
      projectId: "p1",
      requestId: "r1",
    });
    expect(remove).toHaveBeenCalledTimes(2);
  });

  it("stops watching when the view closes without cancelling the server job", async () => {
    const remove = vi.fn();
    const controller = new AbortController();
    const post = vi.fn().mockResolvedValue({
      requestId: "r1",
      state: "running",
      prefix: "demo",
    } satisfies import("./Vault").VaultSetupJob);
    const pending = setupVault(
      { post } as never,
      {
        requestId: "r1",
        projectId: "p1",
        prefix: "demo",
        profile: "fixture",
        region: "eu-central-1",
      },
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

// Compile-time witnesses exercise the public caller and generated outcome owner.
function vaultJobContracts(
  backend: Pick<import("./services").SidebarBackend, "post">,
) {
  // @ts-expect-error Setup cannot receive an unlock request body.
  void setupVault(backend, { projectId: "p1", words: "fixture" });
  // @ts-expect-error A completed pull must carry its counts and destination.
  const missingOutcome: import("./Vault").VaultPull = {
    requestId: "r1",
    state: "done",
    folder: "/fixture/demo",
  };
  void missingOutcome;
  const wrongJob: Extract<Progress, { resource: { job: "pull" } }> = {
    resource: { kind: "vault-job-progress", job: "pull", requestId: "r1" },
    // @ts-expect-error A pull resource cannot carry a login completion.
    value: { requestId: "r1", state: "done" },
  };
  void wrongJob;
}
void vaultJobContracts;
