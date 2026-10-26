import assert from "node:assert/strict";
import { _electron as electron } from "playwright-core";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { fixture } from "../web/tests/servers/multi-server-fixture.mjs";

const root = path.dirname(fileURLToPath(import.meta.url));
const folder = await mkdtemp(path.join(tmpdir(), "studio-native-frames-"));
const remote = await fixture("Remote", true);
let desktop;
try {
  const executablePath = process.env.CODEX_FRAMES_EXECUTABLE;
  desktop = await electron.launch({
    ...(executablePath ? { executablePath } : {}),
    args: [...(executablePath ? [] : [root]), "--hidden", "--ui-only"],
    env: {
      ...process.env,
      CODEX_UI_PORT: "0",
      CODEX_DESKTOP_PROFILE: path.join(folder, "profile"),
      CODEX_AGENTS_STATE_DIR: path.join(folder, "state"),
    },
  });
  await desktop.evaluate(
    ({ safeStorage }, { origin, destination }) => {
      safeStorage.isEncryptionAvailable = () => true;
      safeStorage.encryptString = (value) => Buffer.from(value);
      safeStorage.decryptString = (bytes) => bytes.toString();
      const original = globalThis.fetch;
      globalThis.fetch = async (input, init) => {
        const request = new Request(input, init);
        const url = new URL(request.url);
        if (url.origin !== origin) return original(request);
        const body = await request.clone().arrayBuffer();
        return original(destination + url.pathname + url.search, {
          method: request.method,
          headers: request.headers,
          ...(body.byteLength ? { body } : {}),
          signal: request.signal,
          redirect: "error",
        });
      };
    },
    { origin: remote.invitation.origin, destination: remote.origin },
  );
  const page = await desktop.firstWindow();
  page.setDefaultTimeout(15000);
  assert.equal(
    await desktop.evaluate(({ BrowserWindow }) =>
      BrowserWindow.getAllWindows()[0].isVisible(),
    ),
    false,
  );
  await page.waitForFunction(() => !!window.codexDesktop);
  await page.locator("body").click({ position: { x: 1, y: 1 }, force: true });
  await page.evaluate(async (invitation) => {
    const server = await window.codexDesktop.serverCredentialAction({
      action: "pair",
      origin: invitation.origin,
      invitation,
      requestId: crypto.randomUUID(),
    });
    localStorage.setItem("studio-paired-servers-v1", JSON.stringify([server]));
    localStorage.setItem("studio-selected-server", server.id);
  }, remote.invitation);
  await page.reload();
  const frame = page.frameLocator('iframe[title="Studio on Remote"]');
  await frame.getByRole("button", { name: "New chat", exact: true }).waitFor();
  await frame.getByRole("button", { name: /^Remote chat / }).click();
  await frame.locator("#message").waitFor({ state: "visible" });
  const terminalToggle = frame.getByRole("button", {
    name: "Show terminals",
    exact: true,
  });
  await terminalToggle.waitFor({ state: "visible" });
  const terminalBounds = await terminalToggle.boundingBox();
  const viewportHeight = await page.evaluate(() => window.innerHeight);
  const shellBounds = await page.locator("#root").boundingBox();
  assert.ok(terminalBounds, "The terminal toggle must have visible bounds");
  assert.ok(shellBounds, "The desktop shell must have visible bounds");
  assert.ok(
    terminalBounds.y + terminalBounds.height <=
      shellBounds.y + shellBounds.height,
    `The desktop shell clips the terminal toggle: ${JSON.stringify({ terminalBounds, shellBounds })}`,
  );
  assert.ok(
    terminalBounds.y + terminalBounds.height <= viewportHeight,
    `The terminal toggle is outside the window: ${JSON.stringify({ terminalBounds, viewportHeight })}`,
  );
  await terminalToggle.click();
  await frame
    .getByRole("complementary", { name: "Terminal sessions" })
    .waitFor();
  await frame
    .getByRole("button", { name: "Hide terminals", exact: true })
    .click();
  const frameURL = await page.locator("iframe").getAttribute("src");
  assert.equal(
    new URL(frameURL).searchParams.get("studio-navigation"),
    "classic",
  );
  assert.equal(remote.pairs.length, 1);
  assert.equal(await page.locator("iframe").count(), 1);
  console.log(
    JSON.stringify({
      result: "PASS",
      artifact: executablePath || root,
      hidden: true,
      frame: "classic",
      cases: [
        "native frame navigation",
        "signed server data",
        "sidebar",
        "chat composer",
        "terminal toggle inside the window",
        "terminal panel opens and closes",
      ],
    }),
  );
} finally {
  if (desktop) {
    const pid = desktop.process().pid;
    let watchdog;
    const testChildren = await desktop
      .evaluate(({ app }) => app.getAppMetrics().map(({ pid }) => pid))
      .catch(() => []);
    await Promise.race([
      desktop.close().catch(() => {}),
      new Promise((resolve) => {
        watchdog = setTimeout(() => {
          try {
            process.kill(pid, "SIGKILL");
          } catch {}
          resolve();
        }, 5000);
      }),
    ]);
    clearTimeout(watchdog);
    for (const childPid of testChildren) {
      try {
        const args = execFileSync(
          "ps",
          ["-p", String(childPid), "-o", "args="],
          {
            encoding: "utf8",
          },
        );
        if (args.includes(`--user-data-dir=${path.join(folder, "profile")} `))
          process.kill(childPid, "SIGTERM");
      } catch {}
    }
  }
  await remote.close();
  await rm(folder, { recursive: true, force: true });
}
