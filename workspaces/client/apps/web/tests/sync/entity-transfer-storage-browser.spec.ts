import type { IncomingMessage, ServerResponse } from "node:http";
import type { Connect } from "vite";
import { expect, test } from "../playwright.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("real IndexedDB staging removes crash orphans while preserving another tab's owner lease and cleans cancellation", async ({
  context,
}) => {
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-transfer-storage-"));
  let server;
  try {
    const { createFixtureViteServer: createServer } =
      await import("../playwright.ts");
    server = await createServer({
      cacheDir,
      configFile: false,
      root,
      optimizeDeps: { noDiscovery: true },
      server: { host: "127.0.0.1", port: 0, fs: { allow: [root] } },
    });
    server.middlewares.stack.unshift({
      route: "",
      handle(
        request: IncomingMessage,
        response: ServerResponse,
        next: Connect.NextFunction,
      ) {
        if (request.url !== "/transfer-check") return next();
        response.setHeader("Content-Type", "text/html");
        response.end("<!doctype html><title>Transfer storage fixture</title>");
      },
    });
    await server.listen();
    const address = server.httpServer?.address();
    if (!address || typeof address === "string")
      throw new Error("Fixture address unavailable");

    const first = await context.newPage();
    const second = await context.newPage();
    const url = `http://127.0.0.1:${address.port}/transfer-check`;
    await Promise.all([first.goto(url), second.goto(url)]);
    const owner = await first.evaluate(async () => {
      const { IndexedTransferStorage } =
        await import("/src/sync/entityTransfer.ts");
      const storage = new IndexedTransferStorage();
      await storage.put("owner", "active");
      window.syncEntityTransferStorageBrowserStaging = storage;
      const databases = await indexedDB.databases();
      const name = databases.find((database) =>
        database.name?.startsWith("studio-entity-transfer-"),
      )?.name;
      await new Promise<void>((resolve, reject) => {
        const request = indexedDB.open("studio-entity-transfer-orphan", 1);
        request.onsuccess = () => {
          request.result.close();
          resolve();
        };
        request.onerror = () => reject(request.error);
      });
      return name;
    });
    const status = await second.evaluate(
      async ({ owner }) => {
        const { IndexedTransferStorage } =
          await import("/src/sync/entityTransfer.ts");
        const storage = new IndexedTransferStorage();
        await storage.put("second", "active");
        // Wait for the once-per-owner cleanup's exclusive lock callback rather
        // than polling the database directory.
        await navigator.locks.request(
          "studio-entity-transfer-orphan",
          async () => {},
        );
        const names = (await indexedDB.databases()).map(
          (database) => database.name,
        );
        const value = await storage.get("second");
        await storage.close();
        return {
          ownerPresent: names.includes(owner),
          orphanPresent: names.includes("studio-entity-transfer-orphan"),
          value,
        };
      },
      { owner },
    );
    expect(status).toEqual({
      ownerPresent: true,
      orphanPresent: false,
      value: "active",
    });
    expect(
      await first.evaluate(async () => {
        const storage = window.syncEntityTransferStorageBrowserStaging;
        const value = await storage.get("owner");
        await storage.close();
        return {
          value,
          remaining: (await indexedDB.databases()).filter((database) =>
            database.name?.startsWith("studio-entity-transfer-"),
          ).length,
        };
      }),
    ).toEqual({ value: "active", remaining: 0 });
  } finally {
    await server?.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});

declare global {
  interface Window {
    syncEntityTransferStorageBrowserStaging: import("../../src/sync/entityTransfer.ts").IndexedTransferStorage;
  }
}
