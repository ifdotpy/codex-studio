import assert from "node:assert/strict";
import { _electron as electron } from "playwright-core";
import { mkdtemp, mkdir, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { fixture } from "../tests/client/servers/multi-server-fixture.mjs";

const root = path.dirname(fileURLToPath(import.meta.url));
const folder = await realpath(
  await mkdtemp(path.join(tmpdir(), "studio-native-access-")),
);
const state = path.join(folder, "state");
await mkdir(state);
let unavailable = true;
const remote = await fixture("Remote", true, {
  handle({ url, json }) {
    if (url.pathname === "/api/multi-server/v1/pair" && unavailable) {
      unavailable = false;
      json(
        {
          error: "The invitation is expired or was already used",
          code: "invite_unavailable",
        },
        403,
      );
      return true;
    }
  },
});
const local = await fixture("Home", false, {
  handle({ url, json }) {
    if (url.pathname === "/api/desktop") {
      json({
        application: "codex-agents",
        protocol: 1,
        backendBuild: "a".repeat(64),
        pid: process.pid,
        stateDir: state,
        supervisorMode: true,
      });
      return true;
    }
  },
});
let desktop;
let desktopPid;
try {
  const executablePath = process.env.CODEX_FRAMES_EXECUTABLE;
  desktop = await electron.launch({
    ...(executablePath ? { executablePath } : {}),
    args: [...(executablePath ? [] : [root]), "--hidden"],
    env: {
      ...process.env,
      CODEX_DESKTOP_PORT: new URL(local.origin).port,
      CODEX_DESKTOP_PROFILE: path.join(folder, "profile"),
      CODEX_AGENTS_STATE_DIR: state,
    },
  });
  desktopPid = desktop.process().pid;
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
        const response = await original(
          destination + url.pathname + url.search,
          {
            method: request.method,
            headers: request.headers,
            ...(body.byteLength ? { body } : {}),
            signal: request.signal,
            redirect: "error",
          },
        );
        if (url.pathname.endsWith("/pair") && response.status === 403)
          globalThis.__testUnavailableReceived = true;
        return response;
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
  assert.match(
    await page.evaluate(() =>
      window.codexDesktop
        .serverCredentialAction({
          action: "pair",
          origin: "https://remote.tailnet.ts.net",
          requestId: "manual-without-gesture",
        })
        .catch((error) => error.message),
    ),
    /desktop button/,
  );
  local.discoverPeer(remote);
  await page.reload();
  let rejected = false;
  for (let i = 0; i < 100; i++) {
    rejected = await desktop.evaluate(
      () => !!globalThis.__testUnavailableReceived,
    );
    if (rejected) break;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  assert.equal(
    rejected,
    true,
    "Automatic pairing must reach the native transport without a gesture",
  );
  await new Promise((resolve) => setTimeout(resolve, 100));
  await page.reload();
  await page
    .getByRole("combobox", { name: "Studio server", exact: true })
    .selectOption("remote");
  const frame = page.frameLocator('iframe[title="Studio on Remote"]');
  await frame.getByRole("button", { name: "New chat", exact: true }).waitFor();
  await frame.getByRole("button", { name: /^Remote chat / }).click();
  await frame.locator("#message").waitFor({ state: "visible" });
  const invites = local.accessRequests.filter(
    (row) => row.action === "ui_invite",
  );
  assert.ok(new Set(invites.map((row) => row.requestId)).size >= 2);
  assert.equal(remote.pairs.length, 1);
  assert.equal(
    (await page.locator("body").innerText()).includes("Use a desktop button"),
    false,
  );
  console.log(
    JSON.stringify({
      result: "PASS",
      artifact: executablePath || root,
      hidden: true,
      cases: [
        "automatic access without a gesture",
        "manual pairing still needs a gesture",
        "renewal after confirmed rejection",
        "native chat frame",
      ],
    }),
  );
} finally {
  if (desktop) {
    let watchdog;
    await Promise.race([
      desktop.close().catch(() => {}),
      new Promise((resolve) => {
        watchdog = setTimeout(() => {
          try {
            process.kill(desktopPid, "SIGKILL");
          } catch {}
          resolve();
        }, 5000);
      }),
    ]);
    clearTimeout(watchdog);
  }
  await Promise.all([remote.close(), local.close()]);
  await rm(folder, { recursive: true, force: true });
}
