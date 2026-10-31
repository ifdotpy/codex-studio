import { fileURLToPath } from "node:url";
import { expect, test, createFixtureViteServer } from "../playwright.ts";

test("retired browser copies are removed without losing request identities", async ({
  page,
}) => {
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const address = server.httpServer!.address();
    if (!address || typeof address === "string")
      throw new Error("Missing fixture address");
    await page.route("**/cleanup-check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Cleanup</title>",
      }),
    );
    await page.goto(`http://127.0.0.1:${address.port}/cleanup-check`);
    const result = await page.evaluate(async () => {
      localStorage.setItem("codex-chat-draft:workspace:chat", "old");
      localStorage.setItem("studio-queue-change:scope:chat", "request");
      for (const name of ["codex-studio-dictation", "studio-device-key-v1"]) {
        await new Promise<void>((resolve, reject) => {
          const request = indexedDB.open(name, 1);
          request.onsuccess = () => {
            request.result.close();
            resolve();
          };
          request.onerror = () => reject(request.error);
        });
      }
      const { cleanupLegacyPersistence } =
        await import("/src/browserPersistence.ts");
      await cleanupLegacyPersistence();
      return {
        keys: Object.keys(localStorage),
        databases: (await indexedDB.databases()).map(({ name }) => name).sort(),
      };
    });
    expect(result.keys).toEqual(["studio-queue-change:scope:chat"]);
    expect(result.databases).toEqual(["studio-device-key-v1"]);
  } finally {
    await server.close();
  }
});
