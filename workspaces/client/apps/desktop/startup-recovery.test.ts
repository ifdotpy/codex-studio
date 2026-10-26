import { present } from "./test-support.ts";
import type { BackendIdentity } from "./backend.ts";
import type { RecoveryOptions } from "./recovery.ts";
import { mainTestSource, mainDependency } from "./main-test-source.ts";
import assert from "node:assert/strict";
import vm from "node:vm";
import { test } from "vitest";

const main = mainTestSource();

test("startup uses the verified backend environment before configuring recovery", async () => {
  for (const [existing, expectedEnvironment] of [
    [null, null],
    [
      { restartEnvironment: { CODEX_CANVAS_CWD: "/existing-workspace" } },
      { CODEX_CANVAS_CWD: "/existing-workspace" },
    ],
    [
      { restartEnvironment: {}, supervisorFallback: true },
      { CODEX_AGENTS_SUPERVISOR_MODE: "1" },
    ],
  ] satisfies [Partial<BackendIdentity> | null, NodeJS.ProcessEnv | null][]) {
    const calls: string[] = [];
    let lookup: { origin: string; state: string } | undefined;
    let configured: RecoveryOptions | undefined;
    let finish: (code: number) => void = () => {};
    const stopped = new Promise<number>((resolve) => {
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
      identity: async (origin: string, state: string) => {
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
      configureRecovery: async (options: RecoveryOptions) => {
        calls.push("configureRecovery");
        configured = options;
        return { enabled: true };
      },
    };
    vm.runInNewContext(main, {
      require: (name: string) =>
        name === "electron"
          ? { app, dialog: { showErrorBox() {} } }
          : name === "./backend.ts"
            ? backend
            : name === "./recovery.ts"
              ? recovery
              : name === "./install-mode.ts"
                ? { uiOnlyInstallation: () => false }
                : name.startsWith("./")
                  ? {}
                  : mainDependency(name),
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
      JSON.parse(
        JSON.stringify(present(configured).restartEnvironment ?? null),
      ),
      expectedEnvironment,
    );
  }
});
