import assert from "node:assert/strict";
import { _electron as electron } from "playwright-core";
import { mkdtemp, realpath, rm, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:net";
import { execFileSync, spawnSync } from "node:child_process";
const root = path.dirname(fileURLToPath(import.meta.url));
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
const temp = await mkdtemp(path.join(tmpdir(), "studio-pkg-"));
const state = path.join(temp, "state");
try {
  desktop = await electron.launch({
    executablePath: executable,
    args: ["--hidden"],
    env: {
      ...process.env,
      CODEX_AGENTS_STATE_DIR: state,
      CODEX_DESKTOP_PORT: String(port),
      CODEX_DESKTOP_PROFILE: path.join(temp, "profile"),
      // This fixture owns a fresh state and no supervisor service. Do not
      // inherit the running Studio installation's supervisor mode.
      CODEX_AGENTS_SUPERVISOR_MODE: "0",
      CODEX_AGENTS_PYTHON: "/nonexistent",
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
  const diagnosticsHelp = execFileSync(
    path.join(
      path.dirname(executable),
      "../Resources/workspace/bin/codex-diagnostics",
    ),
    ["--help"],
    { encoding: "utf8" },
  );
  assert.ok(diagnosticsHelp.includes("--port PORT"));
  const peerLink = await realpath(
    path.join(
      path.dirname(executable),
      "../Resources/workspace/bin/studio-peer-link",
    ),
  );
  const peerLinkStart = spawnSync(peerLink, [], {
    input: "",
    encoding: "utf8",
  });
  assert.equal(peerLinkStart.status, 1);
  assert.ok(peerLinkStart.stderr.includes("The configuration is missing"));
  for (const role of ["orchestrator", "subagent"]) {
    const skill = await readFile(
      path.join(
        path.dirname(executable),
        `../Resources/workspace/.agents/skills/codex-${role}/SKILL.md`,
      ),
      "utf8",
    );
    assert.ok(skill.includes(`name: codex-${role}`));
  }
  const identity = await (await fetch(`${origin}/api/desktop`)).json();
  assert.equal(identity.backendKind, "rust");
  pid = identity.pid;
  assert.equal(identity.stateDir, await realpath(state));
  const command = execFileSync(
    "/bin/ps",
    ["-p", String(pid), "-o", "command="],
    { encoding: "utf8" },
  );
  assert.ok(
    command.includes(
      "Codex Studio.app/Contents/Resources/workspace/bin/studio-runtime-server",
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
  try {
    if (desktop) await desktop.close();
  } finally {
    try {
      if (pid) process.kill(pid, "SIGTERM");
    } finally {
      await rm(temp, { recursive: true, force: true });
    }
  }
}
