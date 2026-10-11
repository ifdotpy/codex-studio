// The built site against the real vault function with a fake S3 that holds the writer's fixture
// (workspaces/runtime/apps/vault/test/fixture), in headless Chromium with a virtual authenticator
// that has PRF: the paper key unlocks, a passkey is added and then unlocks alone, a file
// downloads with the writer's content, and a passkey is removed.
//
//   pnpm --filter studio-vault-web run build && node tests/site.e2e.mjs
import { chromium } from "playwright";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync, readdirSync, statSync } from "node:fs";
import http from "node:http";
import { extname, join, relative } from "node:path";
import { makeHandler } from "../../../../runtime/apps/vault/lambda/handler.mjs";
import {
  generateIdentity,
  recipientOf,
  identityFromPaper,
  sealSlot,
  wordsOfIdentity,
} from "../../../../runtime/apps/vault/src/keys.mjs";

const dist = new URL("../dist/", import.meta.url).pathname;
const fixture = new URL(
  "../../../../runtime/apps/vault/test/fixture/",
  import.meta.url,
).pathname;
const vaultIdentity = readFileSync(
  join(fixture, "identity.txt"),
  "utf8",
).trim();
const expected = JSON.parse(
  readFileSync(join(fixture, "expected.json"), "utf8"),
);
const paper = await generateIdentity();

const objects = new Map();
const walk = (dir) =>
  readdirSync(dir).forEach((n) =>
    statSync(join(dir, n)).isDirectory()
      ? walk(join(dir, n))
      : objects.set(
          relative(join(fixture, "objects"), join(dir, n)),
          readFileSync(join(dir, n)),
        ),
  );
walk(join(fixture, "objects"));
objects.set(
  "proj/slots/paper.age",
  Buffer.from(await sealSlot(vaultIdentity, await recipientOf(paper))),
);

const s3 = {
  async send(command) {
    const input = command.input;
    switch (command.constructor.name) {
      case "ListObjectsV2Command":
        return {
          Contents: [...objects.keys()]
            .filter((k) => k.startsWith(input.Prefix))
            .map((Key) => ({ Key })),
        };
      case "GetObjectCommand":
        return {
          Body: { transformToByteArray: async () => objects.get(input.Key) },
        };
      case "PutObjectCommand":
        objects.set(input.Key, Buffer.from(input.Body));
        return {};
      case "DeleteObjectCommand":
        objects.delete(input.Key);
        return {};
    }
    throw new Error(command.constructor.name);
  },
};
let port;
const handler = makeHandler({
  s3,
  env: {
    BUCKET: "b",
    PREFIX: "proj",
    RECIPIENT: await recipientOf(vaultIdentity),
    TOKEN_SECRET: "t".repeat(64),
  },
  presign: async (_client, command) =>
    `http://localhost:${port}/s3/${command.input.Key}`,
});
const types = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".css": "text/css",
  ".wasm": "application/wasm",
};
const server = http.createServer(async (request, response) => {
  const url = new URL(request.url, "http://localhost");
  if (url.pathname.startsWith("/fn/")) {
    let body = "";
    for await (const chunk of request) body += chunk;
    const result = await handler({
      requestContext: { http: { method: request.method } },
      rawPath: url.pathname.slice(3),
      body,
    });
    response.writeHead(result.statusCode, result.headers).end(result.body);
  } else if (url.pathname.startsWith("/s3/")) {
    const data = objects.get(decodeURIComponent(url.pathname.slice(4)));
    response.writeHead(data ? 200 : 404).end(data);
  } else if (url.pathname === "/config.json") {
    response.writeHead(200, { "content-type": "application/json" }).end(
      JSON.stringify({
        functionUrl: `http://localhost:${port}/fn/`,
        prefix: "proj",
      }),
    );
  } else {
    const file = join(dist, url.pathname === "/" ? "index.html" : url.pathname);
    try {
      response
        .writeHead(200, {
          "content-type": types[extname(file)] ?? "application/octet-stream",
        })
        .end(readFileSync(file));
    } catch {
      response.writeHead(404).end();
    }
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
port = server.address().port;

const browser = await chromium.launch();
try {
  const context = await browser.newContext({ acceptDownloads: true });
  const page = await context.newPage();
  const cdp = await context.newCDPSession(page);
  await cdp.send("WebAuthn.enable");
  await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: {
      protocol: "ctap2",
      transport: "internal",
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      hasPrf: true,
      automaticPresenceSimulation: true,
    },
  });
  page.on("pageerror", (error) => console.error("page error:", error.message));
  await page.goto(`http://localhost:${port}/`);
  await page.waitForSelector("#unlock:not([hidden])");
  assert.equal(
    await page.isHidden("#with-passkey"),
    true,
    "no passkey slot yet",
  );

  // The paper key is 24 words. Two words in the wrong order fail the checksum, and the page
  // says so; numbered words, some cut to their first four letters, unlock the latest state.
  const words = wordsOfIdentity(paper).split(" ");
  const swapped = words
    .map((_, i) => {
      const copy = [...words];
      [copy[i], copy[i + 1]] = [copy[i + 1], copy[i]];
      return copy.join(" ");
    })
    .find((text) => {
      try {
        identityFromPaper(text);
        return false;
      } catch {
        return true;
      }
    });
  await page.fill("#paper", swapped);
  await page.click("#paper-form button");
  await page.waitForFunction(() =>
    document.querySelector("#status").textContent.includes("checksum"),
  );
  await page.fill(
    "#paper",
    words
      .map((w, i) => `${i + 1}. ${i % 2 ? w.slice(0, 4).toUpperCase() : w}`)
      .join("\n"),
  );
  await page.click("#paper-form button");
  await page.waitForSelector("#browse:not([hidden])");
  assert.equal(await page.inputValue("#state"), "4");
  await page.click("text=src/");
  const download = page.waitForEvent("download");
  await page.click("text=app.py");
  const data = readFileSync(await (await download).path());
  assert.equal(
    createHash("sha256").update(data).digest("hex"),
    expected.states[3]["src/app.py"][0],
  );

  // An earlier state.
  await page.selectOption("#state", "2");
  await page.waitForSelector("text=empty.txt");

  // A passkey on this device, then a fresh tab that opens with the passkey alone.
  await page.click("#passkeys");
  await page.fill("#label", "test device");
  await page.click("#add-form button");
  await page.waitForSelector("#passkey-list li button");
  assert.ok(
    [...objects.keys()].some((k) => k.startsWith("proj/slots/passkey-")),
  );
  await page.reload();
  await page.waitForSelector("#with-passkey:not([hidden])");
  await page.click("#with-passkey");
  await page.waitForSelector("#browse:not([hidden])");
  assert.equal(await page.inputValue("#state"), "4");

  // Removing the passkey removes its slot; the paper slot stays.
  await page.click("#passkeys");
  await page.click("#passkey-list li button");
  await page.waitForFunction(
    () => !document.querySelector("#passkey-list li button"),
  );
  assert.equal(
    [...objects.keys()].filter((k) => k.startsWith("proj/slots/")).join(),
    "proj/slots/paper.age",
  );
  assert.equal(await page.textContent("#status"), "");
  console.log("site e2e: passed");
} finally {
  await browser.close();
  server.close();
}
