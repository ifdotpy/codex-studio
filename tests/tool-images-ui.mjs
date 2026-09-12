#!/usr/bin/env node
// Real runtime notifications, persisted transcript, HTTP file policy, production App.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { deflateSync } from "node:zlib";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
const root = await mkdtemp(join(tmpdir(), "studio-tool-images-"));
const crc = (bytes) => {
  let value = 0xffffffff;
  for (const byte of bytes) {
    value ^= byte;
    for (let bit = 0; bit < 8; bit++)
      value = (value >>> 1) ^ (value & 1 ? 0xedb88320 : 0);
  }
  return (value ^ 0xffffffff) >>> 0;
};
const chunk = (type, bytes) => {
  const head = Buffer.from(type);
  const length = Buffer.alloc(4);
  length.writeUInt32BE(bytes.length);
  const checksum = Buffer.alloc(4);
  checksum.writeUInt32BE(crc(Buffer.concat([head, bytes])));
  return Buffer.concat([length, head, bytes, checksum]);
};
const header = Buffer.alloc(13);
header.writeUInt32BE(960);
header.writeUInt32BE(640, 4);
header[8] = 8;
header[9] = 2;
const scanline = Buffer.alloc(960 * 3 + 1);
for (let i = 1; i < scanline.length; i += 3) {
  scanline[i] = 25;
  scanline[i + 1] = 120;
  scanline[i + 2] = 180;
}
const png = Buffer.concat([
  Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
  chunk("IHDR", header),
  chunk(
    "IDAT",
    deflateSync(Buffer.concat(Array.from({ length: 640 }, () => scanline))),
  ),
  chunk("IEND", Buffer.alloc(0)),
]);
const imagePath = join(root, "tool image #1%.png");
const slowPath = join(root, "held image.png");
await writeFile(imagePath, png);
await writeFile(slowPath, png);
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
  },
);
let browser,
  page,
  log = "";
fixture.stderr.on("data", (data) => {
  log += data;
});
const until = async (fn, label) => {
  const stop = Date.now() + 15000;
  while (Date.now() < stop) {
    if (await fn()) return;
    await new Promise((resolve) => setTimeout(resolve, 30));
  }
  throw new Error(label);
};
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  browser = await engine.launch({
    headless: true,
    executablePath:
      engine === chromium
        ? process.env.CHROME_BIN ||
          "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        : undefined,
  });
  const context = await browser.newContext({
    viewport: { width: 1100, height: 900 },
    serviceWorkers: "block",
  });
  page = await context.newPage();
  const errors = [],
    fileRequests = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => {
    if (new URL(request.url()).pathname === "/api/file")
      fileRequests.push(new URL(request.url()));
  });
  await page.goto(origin);
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page.locator("#message").fill("Inspect the image fixture");
  await page.locator("#send").click();
  let agent;
  await until(async () => {
    const state = await (await fetch(origin + "/api/state")).json();
    agent = state.runtime.agents.find(
      (agent) => agent.name === "Other project",
    );
    return agent.status === "running" && agent.turnId;
  }, "The isolated turn must start");
  const event = (item) =>
    fixture.stdin.write(
      JSON.stringify({
        method: "item/completed",
        params: { threadId: agent.threadId, turnId: agent.turnId, item },
      }) + "\n",
    );
  const card = (id) =>
    page.locator(`.tool-card[data-message="${agent.id}:${id}"]`);
  event({ type: "imageView", id: "native-image", path: imagePath });
  await card("native-image").waitFor();
  await page.waitForTimeout(200);
  assert.equal(
    fileRequests.length,
    0,
    "Collapsed tools do not read image files",
  );
  await card("native-image").locator(":scope > summary").click();
  await card("native-image")
    .getByRole("img", { name: "tool image #1%.png", exact: true })
    .waitFor();
  await page.waitForFunction(
    () => document.querySelector(".tool-card img")?.naturalWidth === 960,
  );
  assert.equal(fileRequests[0].searchParams.get("agent"), agent.id);
  assert.equal(
    fileRequests[0].searchParams.get("path"),
    imagePath,
    "Raw special characters keep the exact file identity",
  );
  await card("native-image")
    .getByRole("button", { name: "Preview tool image #1%.png", exact: true })
    .click();
  let dialog = page.getByRole("dialog");
  await dialog.waitFor();
  const previewImage = dialog.getByRole("img", {
    name: "tool image #1%.png",
    exact: true,
  });
  await previewImage.waitFor();
  await until(
    () => previewImage.evaluate((element) => element.naturalWidth === 960),
    "The large preview must decode the actual file",
  );
  await dialog.getByRole("button", { name: "Fit", exact: true }).click();
  await until(
    () =>
      previewImage.evaluate((element) => {
        const viewport = element.parentElement;
        const box = element.getBoundingClientRect();
        return (
          element.style.width &&
          box.width <= viewport.clientWidth + 1 &&
          box.height <= viewport.clientHeight + 1
        );
      }),
    "The initial fit must settle before zoom is measured",
  );
  const fittedWidth = await previewImage.evaluate(
    (element) => element.getBoundingClientRect().width,
  );
  await dialog.getByRole("button", { name: "Zoom in", exact: true }).click();
  await until(
    async () =>
      (await previewImage.evaluate(
        (element) => element.getBoundingClientRect().width,
      )) >
      fittedWidth * 1.05,
    "Zoom changes the rendered image size",
  );
  await dialog
    .getByRole("button", { name: "Actual size", exact: true })
    .click();
  await until(
    async () =>
      Math.abs(
        (await previewImage.evaluate(
          (element) => element.getBoundingClientRect().width,
        )) - 960,
      ) < 2,
    "Actual size renders one CSS pixel per image pixel",
  );
  await page.screenshot({ path: join(root, "native-image-zoom.png") });
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden" });
  const beforeInline = fileRequests.length;
  event({
    type: "dynamicToolCall",
    id: "inline-image",
    tool: "image_fixture",
    arguments: {},
    status: "completed",
    success: true,
    contentItems: [
      { type: "inputText", text: "The image is already in this result." },
      {
        type: "inputImage",
        imageUrl: `data:image/png;base64,${png.toString("base64")}`,
      },
    ],
  });
  await card("inline-image").waitFor();
  await card("inline-image").locator(":scope > summary").click();
  await card("inline-image")
    .getByRole("img", { name: "Tool image", exact: true })
    .waitFor();
  assert.equal(
    fileRequests.length,
    beforeInline,
    "Inline content does not read a local file",
  );
  assert.ok(
    !(await card("inline-image").innerText()).includes(png.toString("base64")),
    "Output and raw event do not repeat the image bytes",
  );
  await card("inline-image")
    .getByRole("button", { name: "Preview Tool image", exact: true })
    .click();
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Zoom in", exact: true })
    .click();
  await page.keyboard.press("Escape");
  await page.getByRole("dialog").waitFor({ state: "hidden" });
  let held;
  await page.route("**/api/file?*", (route) => {
    if (new URL(route.request().url()).searchParams.get("path") === slowPath) {
      held = route;
      return;
    }
    return route.fallback();
  });
  event({ type: "imageView", id: "held-image", path: slowPath });
  await card("held-image").waitFor();
  await card("held-image")
    .getByText("View image", { exact: true })
    .click({ position: { x: 8, y: 8 } });
  await until(() => !!held, "The delayed file request must start");
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await held.fulfill({
    json: {
      name: "held image.png",
      mime: "image/png",
      base64: png.toString("base64"),
    },
  });
  await page.waitForTimeout(150);
  assert.equal(
    await page
      .getByRole("img", { name: "held image.png", exact: true })
      .count(),
    0,
    "A late image cannot appear in another chat",
  );
  assert.equal(await page.getByRole("dialog").count(), 0);
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      result: "PASS",
      browser: engine === webkit ? "WebKit" : "Chromium",
      evidence: root,
      checks: [
        "native runtime imageView",
        "lazy exact scoped file request",
        "large zoom preview",
        "inline image without another file read",
        "no raw base64 duplication",
        "cross-chat late response",
      ],
    }),
  );
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
  console.error("Evidence:", root, log);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
  if (fixture.exitCode === null)
    await new Promise((resolve) => fixture.once("exit", resolve));
}
