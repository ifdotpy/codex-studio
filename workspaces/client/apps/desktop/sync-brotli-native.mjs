import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createBrotliCompress, constants } from "node:zlib";
import { chromium, _electron as electron } from "playwright-core";

const root = path.dirname(fileURLToPath(import.meta.url));
const folder = await mkdtemp(path.join(tmpdir(), "studio-sync-brotli-"));
const streams = new Map();
const accepted = [];
const frame = (revision) =>
  `event: ${revision === 2 ? "heartbeat" : "resources"}\nid: ${revision}\ndata: ${JSON.stringify({ revision, text: "é" })}\n\n`;
function produce(stream) {
  stream.encoder.write(frame(++stream.revision));
  stream.encoder.flush(constants.BROTLI_OPERATION_FLUSH);
}
const server = createServer((request, response) => {
  const url = new URL(request.url, "http://localhost");
  if (url.pathname === "/api/sync/stream") {
    assert.match(request.headers["accept-encoding"] || "", /\bbr\b/);
    const client = url.searchParams.get("client");
    if (client === "signed") assert.ok(request.headers["x-studio-signature"]);
    accepted.push(client);
    response.writeHead(200, {
      "Content-Type": "text/event-stream",
      "Content-Encoding": "br",
      "Cache-Control": "no-cache, no-transform",
    });
    const encoder = createBrotliCompress({
      params: {
        [constants.BROTLI_PARAM_QUALITY]: 3,
        [constants.BROTLI_PARAM_LGWIN]: 20,
      },
    });
    const stream = { encoder, revision: 0 };
    streams.set(client, stream);
    encoder.pipe(response);
    response.on("close", () => {
      encoder.destroy();
      streams.delete(client);
    });
    produce(stream);
  } else if (url.pathname === "/advance") {
    const stream = streams.get(url.searchParams.get("client"));
    assert.ok(stream, "the same stream remains open");
    // This request is sent only after the client decodes the preceding event.
    produce(stream);
    response.end("ok");
  } else {
    response.writeHead(200, { "Content-Type": "text/html" });
    response.end("<!doctype html><title>Brotli stream witness</title>");
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const destination = `http://127.0.0.1:${server.address().port}`;
let browser;
let desktop;
try {
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  await page.goto(destination);
  assert.deepEqual(
    await page.evaluate(
      () =>
        new Promise((resolve, reject) => {
          const events = [];
          const source = new EventSource("/api/sync/stream?client=browser");
          const deadline = setTimeout(() => {
            source.close();
            reject(new Error("event buffered until next production"));
          }, 5000);
          function receive(event) {
            const parsed = JSON.parse(event.data);
            events.push([event.type, parsed.revision, parsed.text]);
            if (events.length === 3) {
              clearTimeout(deadline);
              source.close();
              resolve(events);
            } else void fetch("/advance?client=browser").catch(reject);
          }
          source.addEventListener("resources", receive);
          source.addEventListener("heartbeat", receive);
          source.onerror = () => reject(new Error("EventSource failed"));
        }),
    ),
    [
      ["resources", 1, "é"],
      ["heartbeat", 2, "é"],
      ["resources", 3, "é"],
    ],
  );

  const entry = path.join(folder, "main.ts");
  await writeFile(
    entry,
    `const {app,BrowserWindow}=require('electron');globalThis.syncTestCredentials=require(${JSON.stringify(path.join(root, "server-credentials.ts"))});app.setPath('userData',${JSON.stringify(path.join(folder, "profile"))});app.whenReady().then(()=>{const window=new BrowserWindow({show:false});window.loadURL('about:blank');});`,
  );
  desktop = await electron.launch({
    args: [
      entry,
      ...(process.platform === "linux" && !process.env.DISPLAY
        ? ["--ozone-platform=headless"]
        : []),
    ],
  });
  assert.equal(
    await desktop.evaluate(({ BrowserWindow }) =>
      BrowserWindow.getAllWindows()[0].isVisible(),
    ),
    false,
  );
  const actual = await desktop.evaluate(
    async (_electron, { destination, folder }) => {
      const { createServerCredentials } = globalThis.syncTestCredentials;
      const origin = "https://sync-test.ts.net";
      const invitation = {
        protocol: 1,
        serverId: "remote",
        origin,
        inviteId: "fixture",
        token: "fixture",
        publicKey: "fixture",
        tailscaleUser: "owner",
      };
      const adapter = createServerCredentials({
        profile: folder,
        safeStorage: {
          isEncryptionAvailable: () => true,
          encryptString: (text) => Buffer.from(text),
          decryptString: (bytes) => bytes.toString(),
        },
        fetchRequest: async (request) => {
          const url = new URL(request.url);
          if (url.pathname.endsWith("/pair")) {
            const body = await request.json();
            return new Response(
              JSON.stringify({
                ...invitation,
                paired: true,
                clientId: body.clientId,
              }),
            );
          }
          return fetch(destination + url.pathname + url.search, {
            headers: request.headers,
            signal: request.signal,
            redirect: "error",
          });
        },
      });
      const receipt = await adapter.pair({
        origin,
        invitation,
        requestId: "pair",
      });
      const controller = new AbortController();
      const deadline = setTimeout(
        () => controller.abort(new Error("signed stream read deadline")),
        5000,
      );
      const response = await adapter.request(
        {
          serverId: receipt.id,
          credentialId: receipt.credentialId,
          url: origin + "/api/sync/stream?client=signed",
          headers: { Accept: "text/event-stream", "Accept-Encoding": "br" },
        },
        controller.signal,
      );
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      const frames = [];
      let pending = "";
      try {
        while (frames.length < 3) {
          const chunk = await reader.read();
          if (chunk.done) throw new Error("premature stream end");
          pending += decoder.decode(chunk.value, { stream: true });
          const boundary = pending.indexOf("\n\n");
          if (boundary !== -1) {
            frames.push(pending.slice(0, boundary + 2));
            pending = pending.slice(boundary + 2);
            if (frames.length < 3)
              await fetch(destination + "/advance?client=signed");
          }
        }
      } finally {
        clearTimeout(deadline);
        controller.abort();
        await reader.cancel().catch(() => {});
      }
      return frames;
    },
    { destination, folder },
  );
  assert.deepEqual(actual, [frame(1), frame(2), frame(3)]);
  assert.deepEqual(accepted, ["browser", "signed"]);
  console.log(
    JSON.stringify({
      result: "PASS",
      clients: [
        "Chromium EventSource",
        "hidden Electron main-process signed fetch",
      ],
      events: 3,
      flushedBeforeNextProduction: true,
    }),
  );
} finally {
  await desktop?.close();
  await browser?.close();
  server.closeAllConnections();
  await new Promise((resolve) => server.close(resolve));
  await rm(folder, { recursive: true, force: true });
}
