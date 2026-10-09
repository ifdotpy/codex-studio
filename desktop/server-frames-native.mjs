import assert from "node:assert/strict";
import { _electron as electron } from "playwright-core";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { fixture } from "../tests/client/servers/multi-server-fixture.mjs";

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
      ],
    }),
  );
} finally {
  if (desktop) {
    const pid = desktop.process().pid;
    let watchdog;
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
  }
  await remote.close();
  await rm(folder, { recursive: true, force: true });
}
