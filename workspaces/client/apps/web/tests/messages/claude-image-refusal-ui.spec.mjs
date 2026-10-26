import { readFile } from "node:fs/promises";
import path from "node:path";
import {
  test,
  expect,
  createFixtureViteServer,
  apiSchemaHeaders,
  handleEntitySyncFixtureRequest,
  syncIdentityFixture,
} from "../playwright.ts";
import { prepareClaudeImage } from "../../../../../providers/apps/claude-bridge/images.mjs";

test("Claude image refusal remains visible and preserves the draft", async ({
  browser,
}, testInfo) => {
  test.setTimeout(90_000);
  const refusal = await prepareClaudeImage(Buffer.from("damaged image")).then(
    () => {
      throw Error("The fixture must refuse this image");
    },
    (error) => error.message,
  );
  const web = path.resolve(import.meta.dirname, "../..");
  const identity = syncIdentityFixture();
  const syncFixture = {
    workspaceId: identity.workspaceId,
    snapshot: {
      stateDir: "ux-chat-fixture",
      threads: [],
      runtime: {
        agents: [],
        queuesByAgent: {
          lead: {
            items: [],
            revision: "1",
            capabilities: { reorder: false, receipts: false },
          },
        },
      },
    },
  };
  const entry = path.join(web, "tests/fixtures/ux-chat.tsx");
  // Reuse the production Conversation fixture. Its send owner supplies a failed
  // receipt, exactly as App.send does. No backend or model request is needed.
  const source = (await readFile(entry, "utf8"))
    .replace(
      "const [sending, setSending] = useState(false);",
      "const [sending, setSending] = useState(false); const [outgoing,setOutgoing]=useState<any[]>([]);",
    )
    .replace("sending={sending}", "sending={sending} outgoing={outgoing}")
    .replace(
      'await fetch("/api/fixture-send", {',
      `setOutgoing([{id:'image-refusal',body:{room:id,text:getDraft(id),assets:[],delivery:'queue'},status:'failed',created:Date.now(),error:${JSON.stringify(refusal)}}]); throw new Error(${JSON.stringify(refusal)}); await fetch("/api/fixture-send", {`,
    );
  const server = await createFixtureViteServer({
    configFile: false,
    root: web,
    logLevel: "error",
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "image-refusal-conversation",
        enforce: "pre",
        load(id) {
          if (id === entry) return source;
        },
        configureServer(server) {
          server.middlewares.use((request, response, next) => {
            if (handleEntitySyncFixtureRequest(request, response, syncFixture))
              return;
            if (!request.url.startsWith("/api/")) return next();
            for (const [key, value] of Object.entries(apiSchemaHeaders()))
              response.setHeader(key, value);
            response.setHeader("Content-Type", "application/json");
            response.end(
              JSON.stringify({
                items: [],
                data: [],
                nextCursor: null,
                token: "fixture",
              }),
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    for (const width of [320, 1200]) {
      const page = await browser.newPage({ viewport: { width, height: 900 } });
      try {
        await page.route("**/check", (route) =>
          route.fulfill({
            contentType: "text/html",
            body: '<div id="conversation-header-tools"></div><div id="root"></div><script type="module" src="/tests/fixtures/ux-chat.tsx"></script>',
          }),
        );
        await page.goto(server.resolvedUrls.local[0] + "check");
        await page.locator("#message").fill("Read the attached image.");
        await page.locator("#send").click();
        await expect(page.locator(".message-delivery-error")).toContainText(
          refusal,
        );
        await expect(page.locator("#message")).toHaveValue(
          "Read the attached image.",
        );
        await expect(
          page.getByText("The server returned an invalid entity change."),
        ).toHaveCount(0);
        await expect(
          page.getByText("Connection lost. Reconnecting..."),
        ).toHaveCount(0);
        await page.screenshot({
          path: testInfo.outputPath(`image-refusal-${width}.png`),
        });
      } finally {
        await page.close();
      }
    }
  } finally {
    await server.close();
  }
});
