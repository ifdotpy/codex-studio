import { chromium } from "playwright";
import { createServer } from "vite";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";

it("a lost upload enqueue success reuses the original IDs and refuses changed bytes", async () => {
  const server = await createServer({
    root: fileURLToPath(new URL("../../", import.meta.url)),
    configFile: false,
    appType: "custom",
    logLevel: "silent",
    server: {
      host: "127.0.0.1",
      port: 0,
      watch: null,
      hmr: false,
      preTransformRequests: false,
    },
    optimizeDeps: { noDiscovery: true, include: [] },
    plugins: [
      {
        name: "attachment-fixture-injected-defaults",
        enforce: "pre",
        resolveId(source, importer) {
          if (importer?.endsWith("/sync/uploads.ts")) {
            if (["../api", "./client", "./localDraft"].includes(source))
              return "\0attachment-unused-defaults";
            if (source === "../uuid") return "\0attachment-uuid";
            if (source === "../servers/environment")
              return "\0attachment-environment";
            if (source === "../servers/storage") return "\0attachment-storage";
          }
        },
        load(id) {
          if (id === "\0attachment-unused-defaults")
            return 'export function syncGet(){throw Error("Unused default")} export const syncPost=syncGet,refreshSession=syncGet,writeLocalDraft=syncGet,syncDatabase=syncGet;';
          if (id === "\0attachment-uuid")
            return "export const randomUUID=()=>crypto.randomUUID();";
          if (id === "\0attachment-environment")
            return "export const serverStorageName=(name)=>name;";
          if (id === "\0attachment-storage")
            return "export const serverLocalStorage={};";
        },
      },
    ],
  });
  server.middlewares.use("/attachment-fixture", (_request, response) => {
    response.setHeader("Content-Type", "text/html");
    response.end(
      '<!doctype html><title>Attachment receipt fixture</title><script type="module">import {createUploads} from "/src/sync/uploads.ts"; window.fixtureCreateUploads=createUploads;</script>',
    );
  });
  const browser = await chromium.launch({ headless: true });
  try {
    await server.listen();
    const address = server.httpServer.address();
    const page = await browser.newPage();
    page.setDefaultTimeout(3000);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const apiReads = [];
    await page.route("**/api/**", (route) => {
      apiReads.push(route.request().url());
      return route.abort();
    });
    await page.goto(`http://127.0.0.1:${address.port}/attachment-fixture`, {
      waitUntil: "commit",
    });
    try {
      await page.waitForFunction(
        () => typeof window.fixtureCreateUploads === "function",
      );
    } catch {
      throw new Error(
        `Attachment fixture failed to load: ${errors.join("; ")}`,
      );
    }
    const result = await page.evaluate(async () => {
      const createUploads = window.fixtureCreateUploads;
      const workspace = "a".repeat(32),
        values = new Map();
      const uploads = createUploads({
        databaseName: "attachment-replay-fixture",
        storage: {
          getItem: (key) => values.get(key) ?? null,
          setItem: (key, value) => values.set(key, value),
          removeItem: (key) => values.delete(key),
        },
        syncGet: () => {
          throw new Error("No server read is allowed");
        },
        syncPost: () => {
          throw new Error("No server write is allowed");
        },
        refreshSession: () => {
          throw new Error("No session refresh is allowed");
        },
        writeLocalDraft: () => {},
        syncDatabase: async () => ({ workspaceId: workspace }),
      });
      const original = [
        { id: "00000000-0000-4000-8000-000000000001", created: 123 },
      ];
      const file = new File(["original"], "same.txt", { type: "text/plain" });
      const dispatch = window.dispatchEvent.bind(window);
      window.dispatchEvent = () => {
        throw new Error("Lost reply after the IndexedDB commit");
      };
      let lost = false;
      try {
        await uploads.queueUploads(
          "state",
          "agent",
          [file],
          workspace,
          original,
        );
      } catch (error) {
        lost = error.message === "Lost reply after the IndexedDB commit";
      } finally {
        window.dispatchEvent = dispatch;
      }
      const before = await uploads.pendingUploads("state", workspace);
      const replay = await uploads.queueUploads(
        "state",
        "agent",
        [file],
        workspace,
        original,
      );
      const after = await uploads.pendingUploads("state", workspace);
      let refused = false;
      try {
        await uploads.queueUploads(
          "state",
          "agent",
          [new File(["different"], "same.txt", { type: "text/plain" })],
          workspace,
          original,
        );
      } catch {
        refused = true;
      }
      const mismatches = [];
      for (const [state, agent, changed] of [
        ["other", "agent", file],
        ["state", "other", file],
        [
          "state",
          "agent",
          new File(["original"], "renamed.txt", { type: "text/plain" }),
        ],
        [
          "state",
          "agent",
          new File(["original"], "same.txt", { type: "application/json" }),
        ],
        [
          "state",
          "agent",
          new File(["replaced"], "same.txt", { type: "text/plain" }),
        ],
      ]) {
        try {
          await uploads.queueUploads(
            state,
            agent,
            [changed],
            workspace,
            original,
          );
          mismatches.push(false);
        } catch {
          mismatches.push(true);
        }
      }
      // A conflict at the end of a batch must not leave an earlier new row.
      let atomic = false;
      try {
        await uploads.queueUploads(
          "state",
          "agent",
          [file, new File(["replaced"], "same.txt", { type: "text/plain" })],
          workspace,
          [
            { id: "00000000-0000-4000-8000-000000000002", created: 124 },
            ...original,
          ],
        );
      } catch {
        atomic = true;
      }
      const final = await uploads.pendingUploads("state", workspace);
      return {
        lost,
        before,
        replay,
        after,
        refused,
        mismatches,
        atomic,
        final,
      };
    });
    expect(result.lost).toBe(true);
    expect(result.before).toHaveLength(1);
    expect(result.after).toEqual(result.before);
    expect(result.replay).toEqual(result.before);
    expect(result.before[0]).toMatchObject({
      id: "00000000-0000-4000-8000-000000000001",
      created: 123,
    });
    expect(result.refused).toBe(true);
    expect(result.mismatches).toEqual([true, true, true, true, true]);
    expect(result.atomic).toBe(true);
    expect(result.final).toEqual(result.before);
    expect(apiReads).toEqual([]);
  } finally {
    await browser.close();
    await server.close();
  }
}, 15000);
