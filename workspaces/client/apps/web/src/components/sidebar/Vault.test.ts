import { describe, expect, it } from "vitest";
import {
  chipWithVault,
  deskLink,
  pullText,
  pullVault,
  vaultLabel,
  type ProjectVault,
  type VaultPull,
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

  it("asks about one pull with one request id until it ends", async () => {
    const bodies: unknown[] = [];
    const replies: VaultPull[] = [
      { requestId: "r1", state: "running", folder: "/Downloads/codex-agents" },
      { requestId: "r1", state: "running", folder: "/Downloads/codex-agents" },
      {
        requestId: "r1",
        state: "done",
        folder: "/Downloads/codex-agents",
        seq: 12,
        files: 340,
        bytes: 1000,
      },
    ];
    const backend = {
      post: (async (path: string, body: unknown) => {
        expect(path).toBe("/api/vault/pull");
        bodies.push(body);
        return replies.shift();
      }) as never,
    };
    const job = await pullVault(backend, "p1", "/Downloads", {
      requestId: "r1",
      wait: async () => {},
    });
    expect(bodies).toEqual(
      Array(3).fill({ projectId: "p1", requestId: "r1", to: "/Downloads" }),
    );
    expect(pullText(job)).toBe(
      "Pulled state 12 (340 files) into /Downloads/codex-agents",
    );
    expect(
      pullText({
        requestId: "r2",
        state: "failed",
        folder: "x",
        error: "the vault codex-agents is locked on this Mac",
      }),
    ).toBe("The pull failed: the vault codex-agents is locked on this Mac");
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
});
