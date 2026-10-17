import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
  FIXTURE_OPTIMIZE_DEPS,
} from "../playwright.mjs";

test("upload owners retain distinct bytes and reject a late cancelled attachment", async ({
  page,
}) => {
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const cache = await mkdtemp(join(tmpdir(), "studio-upload-owners-"));
  const server = await createFixtureViteServer({
    configFile: false,
    root,
    cacheDir: cache,
    optimizeDeps: { noDiscovery: true, include: FIXTURE_OPTIMIZE_DEPS.include },
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  try {
    await server.listen();
    await page.route("**/owner-check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Owner check</title>",
      }),
    );
    await page.goto(`${server.resolvedUrls.local[0]}owner-check`);
    const result = await page.evaluate(async () => {
      const { createUploads } = await import("/src/sync/uploads.ts");
      const { scopedStorage } = await import("/src/servers/storage.ts");
      const { createLocalDraftStorage } =
        await import("/src/sync/localDraft.ts");
      const sent = [];
      let finishA, startedA;
      const started = new Promise((resolve) => {
        startedA = resolve;
      });
      const make = (owner) => {
        const storage = scopedStorage(() => localStorage, `${owner}:`);
        return createUploads({
          databaseName: `uploads-${owner}`,
          storage,
          writeLocalDraft: createLocalDraftStorage(storage, `drafts-${owner}`)
            .writeLocalDraft,
          syncDatabase: async () => ({ workspaceId: "same-workspace" }),
          syncGet: async () => ({ workspaceId: "same-workspace" }),
          refreshSession: async () => ({ token: owner }),
          syncPost: async (path, body, options) => {
            sent.push({ owner, path, body, options });
            const asset = {
              id: body.id,
              name: body.name,
              path: `/${owner}/asset`,
              size: 3,
              mime: "text/plain",
            };
            if (owner === "a") {
              startedA();
              return new Promise((resolve) => {
                finishA = () => resolve(asset);
              });
            }
            return asset;
          },
        });
      };
      const a = make("a"),
        b = make("b");
      const uuid = crypto.randomUUID;
      let rowA, rowB;
      try {
        crypto.randomUUID = () => "same-file-id";
        [rowA] = await a.queueUploads(
          "same-state",
          "same-chat",
          [new File(["AAA"], "same.txt")],
          "same-workspace",
        );
        [rowB] = await b.queueUploads(
          "same-state",
          "same-chat",
          [new File(["BBB"], "same.txt")],
          "same-workspace",
        );
      } finally {
        crypto.randomUUID = uuid;
      }
      const pendingA = a.deliverUpload(rowA);
      await started;
      const assetB = await b.deliverUpload(rowB);
      await a.removeUpload(rowA.id);
      finishA();
      const lateA = await pendingA;
      return {
        sent,
        lateA,
        assetB,
        cancelledA: a.attachmentCancelled(rowA.id),
        cancelledB: b.attachmentCancelled(rowB.id),
        rowsA: await a.pendingUploads("same-state"),
        rowsB: await b.pendingUploads("same-state"),
      };
    });
    expect(result.lateA).toBeNull();
    expect(result.cancelledA).toBe(true);
    expect(result.cancelledB).toBe(false);
    expect(result.rowsA).toEqual([]);
    expect(result.rowsB).toHaveLength(1);
    expect(result.assetB.path).toBe("/b/asset");
    expect(
      result.sent.map((row) => [
        row.owner,
        row.body.id,
        row.body.base64,
        row.options.sessionToken,
      ]),
    ).toEqual([
      ["a", "same-file-id", "QUFB", "a"],
      ["b", "same-file-id", "QkJC", "b"],
    ]);
  } finally {
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
