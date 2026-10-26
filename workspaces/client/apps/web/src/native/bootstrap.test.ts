import { beforeEach, expect, it, vi } from "vitest";
import contract from "../../../../contracts/native-migration.json";

const fixture = vi.hoisted(() => ({
  complete: false,
  calls: [] as string[],
  migrate: vi.fn(),
  verify: vi.fn(),
}));

function proof(serverId: string) {
  return {
    serverId,
    sourceIdentity: "a".repeat(64),
    proofVersion: contract.proofVersion,
    sourceDigest: "c".repeat(64),
    inventoryDigest: "d".repeat(64),
  };
}

vi.mock("./client", () => ({
  nativeStorageCutover: vi.fn(async ({ id }: { id: string }) => {
    fixture.calls.push(`guard:${id}`);
    return {
      serverId: id,
      sourceIdentity: "a".repeat(64),
      migrationComplete: fixture.complete,
      verify: () => fixture.verify(id),
      migrate: () => fixture.migrate(id),
    };
  }),
  nativeServerContext: vi.fn(async ({ id }: { id: string }) => ({
    serverId: id,
    workspaceId: "b".repeat(32),
  })),
}));
vi.mock("./preferenceStore", () => ({
  openNativePreferences: vi.fn(async ({ serverId }: { serverId: string }) => {
    fixture.calls.push(`preferences:${serverId}`);
    return { flush: async () => {} };
  }),
}));
import { nativeBootstrap } from "./bootstrap";

beforeEach(() => {
  fixture.complete = false;
  fixture.calls = [];
  fixture.verify.mockReset().mockImplementation(async (id: string) => ({
    ...proof(id),
    verified: true,
    hostVerified: true,
  }));
  fixture.migrate.mockReset().mockImplementation(async (id: string) => ({
    ...proof(id),
    complete: true,
    owners: [{ ...proof(id), workspaceId: "b".repeat(32), complete: true }],
  }));
});

it("coalesces each selected owner without sharing another server's ports", async () => {
  const root = await nativeBootstrap({ report: vi.fn() });
  const [one, two] = await Promise.all([
    root.migrateServer({ id: "remote" }),
    root.migrateServer({ id: "remote" }),
  ]);
  expect(one).toBe(two);
  expect(one.context.serverId).toBe("remote");
  expect(one.storage).not.toBe(root.storage);
  expect(fixture.calls.filter((step) => step === "guard:remote")).toHaveLength(
    1,
  );
});

it("uses a private host seal without importing the same source again", async () => {
  fixture.complete = true;
  const ready = await nativeBootstrap({ report: vi.fn() });
  expect(ready.migration).toEqual(proof("local"));
  expect(fixture.migrate).not.toHaveBeenCalled();
  expect(fixture.calls).toEqual(["guard:local", "preferences:local"]);
});

it("keeps archive conflicts held and allows a fresh attempt after a proved rejection", async () => {
  const root = await nativeBootstrap({ report: vi.fn() });
  fixture.migrate.mockRejectedValueOnce(
    new Error("The saved archive conflicts with the current source."),
  );
  await expect(root.migrateServer({ id: "remote" })).rejects.toThrow(
    "archive conflicts",
  );
  expect(fixture.calls).not.toContain("preferences:remote");
  await expect(root.migrateServer({ id: "remote" })).resolves.toMatchObject({
    context: { serverId: "remote" },
  });
  expect(fixture.calls.filter((step) => step === "guard:remote")).toHaveLength(
    2,
  );
});

it("holds UI writes if verification changes after the import", async () => {
  fixture.verify.mockResolvedValueOnce({
    ...proof("local"),
    inventoryDigest: "e".repeat(64),
    verified: true,
    hostVerified: true,
  });
  await expect(nativeBootstrap({ report: vi.fn() })).rejects.toThrow("changed");
  expect(fixture.calls).not.toContain("preferences:local");
});

it("captures the selector before the caller changes its object", async () => {
  const owner = { id: "local" };
  const pending = nativeBootstrap({ owner, report: vi.fn() });
  owner.id = "foreign";
  const ready = await pending;
  expect(ready.context.serverId).toBe("local");
  expect(ready.migration.serverId).toBe("local");
});
