import { fileURLToPath } from "node:url";
import type { AddressInfo } from "node:net";
import { expect, test } from "../playwright";

test("queue identity lock preserves the first request across tabs", async ({
  context,
}) => {
  const { createFixtureViteServer } = await import("../playwright");
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const pages = await Promise.all([context.newPage(), context.newPage()]);
    await context.route("**/check", (route) =>
      route.fulfill({ contentType: "text/html", body: "<!doctype html>" }),
    );
    const origin = `http://127.0.0.1:${(server.httpServer!.address() as AddressInfo).port}`;
    await Promise.all(pages.map((page) => page.goto(origin + "/check")));
    const results = await Promise.all(
      pages.map((page, index) =>
        page.evaluate(async (index) => {
          const { createQueueOutbox } =
            await import("/src/browserPersistence.ts");
          const update = createQueueOutbox(localStorage, "queue-outbox-test");
          try {
            return await update<{ request_id: string } | null>(
              "scope:agent",
              null,
              (current) => {
                if (current !== null) throw new Error("pending");
                return { request_id: `request-${index}` };
              },
            );
          } catch {
            return null;
          }
        }, index),
      ),
    );
    expect(results.filter(Boolean)).toHaveLength(1);
    expect(
      JSON.parse(
        (await pages[0]!.evaluate(() =>
          localStorage.getItem("studio-queue-change:scope:agent"),
        ))!,
      ),
    ).toEqual(results.find(Boolean));
  } finally {
    await server.close();
  }
});
