import assert from "node:assert/strict";
import { _electron as electron } from "playwright-core";
import { mkdtemp, realpath } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:net";
import { execFileSync } from "node:child_process";
const root = path.dirname(fileURLToPath(import.meta.url));
const temp = await mkdtemp(path.join(tmpdir(), "codex-desktop-package-test-"));
const state = path.join(temp, "state");
const port = await new Promise((resolve) => {
  const server = createServer();
  server.listen(0, "127.0.0.1", () => {
    const port = server.address().port;
    server.close(() => resolve(port));
  });
});
const executable =
  process.env.CODEX_STUDIO_ELECTRON ||
  path.join(
    root,
    "dist/Codex Studio-darwin-arm64/Codex Studio.app/Contents/MacOS/Codex Studio",
  );
const origin = `http://127.0.0.1:${port}`;
let desktop;
let pid;
try {
  desktop = await electron.launch({
    executablePath: executable,
    args: ["--hidden"],
    env: {
      ...process.env,
      CODEX_AGENTS_STATE_DIR: state,
      CODEX_DESKTOP_PORT: String(port),
      CODEX_DESKTOP_PROFILE: path.join(temp, "profile"),
      CODEX_BOARD_STATE_DIR: path.join(temp, "board"),
    },
  });
  const page = await desktop.firstWindow();
  await page.waitForFunction(
    () => !!window.codexDesktop && !!document.querySelector("textarea"),
  );
  assert.equal(
    await page.evaluate(() =>
      ["requestMicrophone", "prepareTranscription", "transcribeAudio"].every(
        (method) => typeof window.codexDesktop[method] === "function",
      ),
    ),
    true,
  );
  const speech = JSON.parse(
    execFileSync(
      path.join(path.dirname(executable), "../Resources/studio-speech"),
      ["--check"],
      { encoding: "utf8" },
    ),
  );
  assert.equal(speech.helperReady, true);
  assert.equal(speech.onDeviceOnly, true);
  const identity = await (await fetch(`${origin}/api/desktop`)).json();
  pid = identity.pid;
  assert.equal(identity.stateDir, await realpath(state));
  const command = execFileSync(
    "/bin/ps",
    ["-p", String(pid), "-o", "command="],
    { encoding: "utf8" },
  );
  assert.ok(
    command.includes(
      "Codex Studio.app/Contents/Resources/workspace/scripts/codex-canvas",
    ),
  );
  assert.equal(await desktop.evaluate(({ app }) => app.isPackaged), true);
  assert.equal(
    await desktop.evaluate(({ BrowserWindow }) =>
      BrowserWindow.getAllWindows()[0].isVisible(),
    ),
    false,
  );
  await page.screenshot({ path: path.join(temp, "packaged-desktop.png") });
  await desktop.close();
  desktop = null;
  assert.equal((await (await fetch(`${origin}/api/desktop`)).json()).pid, pid);
  console.log(
    JSON.stringify({
      result: "PASS",
      artifact: executable,
      evidence: temp,
      backendPersistsAfterQuit: true,
    }),
  );
} finally {
  if (desktop) await desktop.close();
  if (pid) process.kill(pid, "SIGTERM");
}
