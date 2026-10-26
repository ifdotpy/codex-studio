import { beforeEach, expect, it, vi } from "vitest";
import contract from "../../../../contracts/native-migration.json";

const fixture = vi.hoisted(() => ({
  steps: [] as string[],
  complete: false,
  hostVerified: false,
  blocked: false,
  migrate: vi.fn(),
  verify: vi.fn(),
  browserRead: vi.fn(),
}));

vi.mock("./client", () => ({
  nativeStorageCutover: vi.fn(async (owner: { id: string }) => {
    if (fixture.blocked)
      throw new Error("The source is still open for writes.");
    return {
      serverId: owner.id,
      sourceIdentity: "a".repeat(64),
      migrationComplete: fixture.complete,
      archive: null,
      migrate: fixture.migrate,
      verify: fixture.verify,
    };
  }),
  nativeServerContext: vi.fn(async (owner: { id: string }) => {
    fixture.steps.push(`context:${owner.id}`);
    return { serverId: owner.id, workspaceId: "b".repeat(32) };
  }),
  prepareLegacyNativePreferences: vi.fn(),
}));
vi.mock("./storageMigration", () => ({
  captureLegacyNativeStorage: () => fixture.browserRead(),
  readPendingLegacyPreferences: vi.fn(),
  migrateNativeStorage: vi.fn(),
}));
vi.mock("./preferenceStore", () => ({
  openNativePreferences: vi.fn(async (context: { serverId: string }) => {
    fixture.steps.push(`preferences:${context.serverId}`);
    return { flush: vi.fn() };
  }),
}));

import { nativeBootstrap } from "./bootstrap";

function proof(serverId = "local") {
  return {
    serverId,
    sourceIdentity: "a".repeat(64),
    complete: true,
    proofVersion: contract.proofVersion,
    sourceDigest: "c".repeat(64),
    inventoryDigest: "d".repeat(64),
    owners: [
      {
        serverId,
        workspaceId: "b".repeat(32),
        sourceIdentity: "a".repeat(64),
        complete: true,
        proofVersion: contract.proofVersion,
        sourceDigest: "c".repeat(64),
        inventoryDigest: "d".repeat(64),
      },
    ],
  };
}

beforeEach(() => {
  fixture.steps = [];
  fixture.complete = false;
  fixture.hostVerified = false;
  fixture.blocked = false;
  fixture.browserRead.mockReset().mockImplementation(() => {
    throw new Error("The UI must not read legacy browser storage.");
  });
  fixture.migrate.mockReset().mockImplementation(async () => {
    fixture.steps.push("host-migrate");
    fixture.hostVerified = true;
    return proof();
  });
  fixture.verify.mockReset().mockImplementation(async () => {
    fixture.steps.push("host-verify");
    if (fixture.blocked)
      throw new Error("The source is still open for writes.");
    return { ...proof(), verified: true, hostVerified: fixture.hostVerified };
  });
});

it("opens native UI ports only after the host verifies all saved data", async () => {
  const ready = await nativeBootstrap({ report: vi.fn() });
  expect(ready.context.serverId).toBe("local");
  expect(fixture.migrate).toHaveBeenCalledOnce();
  expect(fixture.verify).toHaveBeenLastCalledWith(
    expect.objectContaining({
      proofVersion: contract.proofVersion,
      sourceDigest: "c".repeat(64),
      inventoryDigest: "d".repeat(64),
    }),
  );
  expect(fixture.steps.indexOf("host-migrate")).toBeLessThan(
    fixture.steps.indexOf("preferences:local"),
  );
  expect(fixture.steps.lastIndexOf("host-verify")).toBeLessThan(
    fixture.steps.indexOf("context:local"),
  );
  expect(fixture.browserRead).not.toHaveBeenCalled();
});

it("migrates a new source before requesting its completion proof", async () => {
  fixture.verify.mockImplementation(async (expected) => {
    if (!expected)
      throw new Error("The host has no existing migration proof.");
    fixture.steps.push("host-verify");
    return { ...proof(), verified: true, hostVerified: true };
  });
  await expect(nativeBootstrap({ report: vi.fn() })).resolves.toMatchObject({
    context: { serverId: "local" },
  });
  expect(fixture.verify).toHaveBeenCalledOnce();
  expect(fixture.steps.indexOf("host-migrate")).toBeLessThan(
    fixture.steps.indexOf("host-verify"),
  );
  expect(fixture.steps.indexOf("host-verify")).toBeLessThan(
    fixture.steps.indexOf("preferences:local"),
  );
});

it("refuses an old complete flag without a host capture proof", async () => {
  fixture.complete = true;
  await expect(nativeBootstrap({ report: vi.fn() })).rejects.toThrow(
    /verif|proof/i,
  );
  expect(fixture.steps).not.toContain("preferences:local");
  expect(fixture.browserRead).not.toHaveBeenCalled();
});

it.each(["sourceDigest", "inventoryDigest", "sourceIdentity"])(
  "keeps writes held when the host completion has the wrong %s",
  async (field) => {
    fixture.migrate.mockResolvedValue({ ...proof(), [field]: "not-a-proof" });
    await expect(nativeBootstrap({ report: vi.fn() })).rejects.toThrow();
    expect(fixture.steps).not.toContain("preferences:local");
    expect(fixture.browserRead).not.toHaveBeenCalled();
  },
);

it("keeps ports closed when an inventoried owner has incomplete bytes", async () => {
  const result = proof();
  result.owners[0].complete = false;
  fixture.migrate.mockResolvedValue(result);
  await expect(nativeBootstrap({ report: vi.fn() })).rejects.toThrow();
  expect(fixture.steps).not.toContain("preferences:local");
});

it("stops before migration when the source guard cannot exclude an old writer", async () => {
  fixture.blocked = true;
  await expect(nativeBootstrap({ report: vi.fn() })).rejects.toThrow(
    "still open",
  );
  expect(fixture.migrate).not.toHaveBeenCalled();
  expect(fixture.browserRead).not.toHaveBeenCalled();
});
