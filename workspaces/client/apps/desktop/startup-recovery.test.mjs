import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import vm from "node:vm";
import { test } from "vitest";

const require = createRequire(import.meta.url);
const main = readFileSync(new URL("./main.cjs", import.meta.url), "utf8");

test("startup uses the verified backend environment before configuring recovery", async () => {
  for (const existing of [
    null,
    { restartEnvironment: { CODEX_CANVAS_CWD: "/existing-workspace" } },
    { restartEnvironment: {}, supervisorFallback: true },
  ]) {
    const calls = [];
    let lookup, configured;
    let finish;
    const stopped = new Promise((resolve) => {
      finish = resolve;
    });
    const app = {
      isPackaged: true,
      setPath() {},
      getPath: () => "/fixture-profile",
      getAppPath: () =>
        "/Applications/Codex Studio.app/Contents/Resources/app.asar",
      setName() {},
      enableSandbox() {},
      requestSingleInstanceLock: () => true,
      whenReady: () => Promise.resolve(),
      on() {},
      exit: finish,
    };
    const backend = {
      identity: async (origin, state) => {
        lookup = { origin, state };
        calls.push("identity");
        return existing;
      },
      ensureBackend: async () => {
        calls.push("ensureBackend");
        throw new Error("fixture stops before creating a window");
      },
    };
    const recovery = {
      isInstalledApplication: () => true,
      trackDesktopRecovery: () => ({}),
      recoveryPaths: () => ({ state: "/canonical-state" }),
      recoveryPreference: () => true,
      supervisorPreference: () => true,
      configureRecovery: async (options) => {
        calls.push("configureRecovery");
        configured = options;
        return { enabled: true };
      },
    };
    vm.runInNewContext(main, {
      require: (name) =>
        name === "electron"
          ? { app, dialog: { showErrorBox() {} } }
          : name === "./backend.cjs"
            ? backend
            : name === "./recovery.cjs"
              ? recovery
              : name === "./install-mode.cjs"
                ? { uiOnlyInstallation: () => false }
                : name.startsWith("./")
                  ? {}
                  : require(name),
      process: {
        argv: [],
        platform: "darwin",
        resourcesPath: "/fixture-resources",
        env: {
          CODEX_HOME: "/foreign-command-executor",
          CODEX_DESKTOP_PORT: "4721",
        },
      },
      module: { exports: {} },
      console: { error() {} },
    });
    assert.equal(await stopped, 1);
    assert.deepEqual(calls, ["identity", "configureRecovery", "ensureBackend"]);
    assert.deepEqual(lookup, {
      origin: "http://127.0.0.1:4721",
      state: "/canonical-state",
    });
    assert.deepEqual(
      JSON.parse(JSON.stringify(configured.restartEnvironment ?? null)),
      existing?.supervisorFallback
        ? { ...existing.restartEnvironment, CODEX_AGENTS_SUPERVISOR_MODE: "1" }
        : (existing?.restartEnvironment ?? null),
    );
  }
});
