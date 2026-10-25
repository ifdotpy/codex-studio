import { expect, it } from "vitest";
import { chromium } from "playwright";
import { createServer } from "vite";
import { fileURLToPath } from "node:url";

it("real IndexedDB staging removes crash orphans while preserving another tab's owner lease and cleans cancellation", async () => {
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const server = await createServer({
    configFile: false,
    root,
    server: { host: "127.0.0.1", port: 0, fs: { allow: [root] } },
  });
  server.middlewares.use((request, response, next) => {
    if (request.url !== "/transfer-check") return next();
    response.setHeader("Content-Type", "text/html");
    response.end("<!doctype html><title>Transfer storage fixture</title>");
  });
  await server.listen();
  const address = server.httpServer!.address();
  if (!address || typeof address === "string")
    throw new Error("Fixture address unavailable");
  const browser = await chromium.launch({ headless: true });
  try {
    const context = await browser.newContext();
    const first = await context.newPage();
    const second = await context.newPage();
    const url = `http://127.0.0.1:${address.port}/transfer-check`;
    await Promise.all([first.goto(url), second.goto(url)]);
    const module = `/@fs${fileURLToPath(new URL("./entityTransfer.ts", import.meta.url))}`;
    const owner = await first.evaluate(async (module) => {
      const { IndexedTransferStorage } = await new Function(
        "path",
        "return import(path)",
      )(module);
      const storage = new IndexedTransferStorage();
      await storage.put("owner", "active");
      (window as unknown as { staging: typeof storage }).staging = storage;
      const databases = await indexedDB.databases();
      const name = databases.find((database) =>
        database.name?.startsWith("studio-entity-transfer-"),
      )!.name!;
      await new Promise<void>((resolve, reject) => {
        const request = indexedDB.open("studio-entity-transfer-orphan", 1);
        request.onsuccess = () => {
          request.result.close();
          resolve();
        };
        request.onerror = () => reject(request.error);
      });
      return name;
    }, module);
    const status = await second.evaluate(
      async ({ module, owner }) => {
        const { IndexedTransferStorage } = await new Function(
          "path",
          "return import(path)",
        )(module);
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
      { module, owner },
    );
    expect(status).toEqual({
      ownerPresent: true,
      orphanPresent: false,
      value: "active",
    });
    expect(
      await first.evaluate(async () => {
        const storage = (
          window as unknown as {
            staging: {
              get(key: string): Promise<string>;
              close(): Promise<void>;
            };
          }
        ).staging;
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
    await context.close();
  } finally {
    await browser.close();
    await server.close();
  }
}, 30000);
