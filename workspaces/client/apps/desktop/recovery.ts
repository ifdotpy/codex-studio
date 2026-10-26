import { execFileSync } from "node:child_process";
import type { App } from "electron";
import type { SavedRecovery } from "./restart-environment.ts";
export type { SavedRecovery } from "./restart-environment.ts";
export type RecoveryRun = (
  file: string,
  args: string[],
) => Promise<{ stdout?: string } | void>;
export type RecoveryOptions = {
  applicationPath: string;
  resources: string;
  supervisor: string;
  enabled: boolean;
  port?: number;
  env?: NodeJS.ProcessEnv;
  restartEnvironment?: NodeJS.ProcessEnv | undefined;
  home?: string;
  uid?: number;
  run?: RecoveryRun;
  checkInterpreter?: typeof checkRecoveryInterpreter;
};
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
const installedApplicationPath =
  "/Applications/Codex Studio.app/Contents/Resources/app.asar";
import { executable, stateDirectory, runtimeServer } from "./backend.ts";
import {
  readRecoveryConfig,
  restartKeys,
  savedLaunchEnvironment,
} from "./restart-environment.ts";
const runFile = promisify(execFile);

function processSupervisor(resources: string, env: NodeJS.ProcessEnv) {
  const binary =
    env.CODEX_PROCESS_SUPERVISOR ||
    path.join(resources, "bin", "codex-process-supervisor");
  if (!path.isAbsolute(binary))
    throw new Error("The Rust process supervisor path must be absolute.");
  fs.accessSync(binary, fs.constants.X_OK);
  return binary;
}

// Milliseconds that one interpreter check may take (15 s).
const INTERPRETER_CHECK_MS = 15000;
// The packaged recover_backend.py imports enum.StrEnum, so it needs Python 3.11 or newer.
// An interpreter counts only when it runs the script's own check (its imports and helpers).
async function checkRecoveryInterpreter(python: string, script: string) {
  await runFile(python, ["-B", script, "--check-interpreter"], {
    timeout: INTERPRETER_CHECK_MS,
  });
}
// Every python3 on PATH in order, then the saved interpreter and the usual install folders.
function interpreterCandidates(env: NodeJS.ProcessEnv, saved: SavedRecovery) {
  const candidates: string[] = [];
  const add = (file: string) => {
    if (!path.isAbsolute(file) || candidates.includes(file)) return;
    try {
      fs.accessSync(file, fs.constants.X_OK);
      candidates.push(file);
    } catch {}
  };
  for (const dir of (env.PATH || "").split(path.delimiter))
    if (dir) add(path.join(dir, "python3"));
  if (typeof saved.python === "string") add(saved.python);
  for (const dir of ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"])
    add(path.join(dir, "python3"));
  return candidates;
}
async function interpreterRuns(
  checkInterpreter: typeof checkRecoveryInterpreter,
  python: string,
  script: string,
) {
  try {
    await checkInterpreter(python, script);
    return true;
  } catch {
    return false;
  }
}
// Never save or register an interpreter that cannot run the recovery script.
async function supportedPython({
  env,
  saved,
  script,
  checkInterpreter,
}: {
  env: NodeJS.ProcessEnv;
  saved: SavedRecovery;
  script: string;
  checkInterpreter: typeof checkRecoveryInterpreter;
}) {
  const refused = [];
  for (const candidate of interpreterCandidates(env, saved)) {
    if (await interpreterRuns(checkInterpreter, candidate, script))
      return candidate;
    refused.push(candidate);
  }
  throw new Error(
    `Background recovery needs Python 3.11 or newer to run ${path.basename(script)}. ` +
      `${refused.length ? `These interpreters cannot run it: ${refused.join(", ")}.` : "No python3 was found."} ` +
      "Install Python 3.11 or newer, for example with Homebrew, and try again.",
  );
}

function rendererIndex(resources: string) {
  const development = path.join(
    resources,
    "workspaces/client/apps/web/dist/index.html",
  );
  return fs.existsSync(development)
    ? development
    : path.join(resources, "web/dist/index.html");
}

function atomicJSON(filename: string, data: unknown) {
  fs.mkdirSync(path.dirname(filename), { recursive: true });
  const temporary = `${filename}.${process.pid}.tmp`;
  const fd = fs.openSync(temporary, "w", 0o600);
  try {
    fs.writeFileSync(fd, `${JSON.stringify(data, null, 2)}\n`);
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
  fs.renameSync(temporary, filename);
  const dir = fs.openSync(path.dirname(filename), "r");
  try {
    fs.fsyncSync(dir);
  } finally {
    fs.closeSync(dir);
  }
}
function recoveryPaths(env = process.env, home = os.homedir()) {
  const state = stateDirectory(env);
  fs.mkdirSync(state, { recursive: true });
  const canonical = fs.realpathSync(state);
  const suffix = createHash("sha256")
    .update(canonical)
    .digest("hex")
    .slice(0, 16);
  return {
    state: canonical,
    config: path.join(canonical, "background-recovery.json"),
    label: `local.codex.agents.recovery.${suffix}`,
    plist: path.join(
      home,
      "Library/LaunchAgents",
      `local.codex.agents.recovery.${suffix}.plist`,
    ),
    supervisorLabel: `local.codex.agents.supervisor.${suffix}`,
    supervisorPlist: path.join(
      home,
      "Library/LaunchAgents",
      `local.codex.agents.supervisor.${suffix}.plist`,
    ),
  };
}
function recoveryPreference(env = process.env) {
  const { config } = recoveryPaths(env);
  try {
    return readRecoveryConfig(config).enabled === true;
  } catch (caughterror) {
    if (!(caughterror instanceof Error)) throw caughterror;
    const error: NodeJS.ErrnoException = caughterror;
    if (error.code === "ENOENT") return true;
    throw error;
  }
}
function supervisorPreference(env = process.env) {
  if (env.CODEX_AGENTS_SUPERVISOR_MODE !== undefined)
    return env.CODEX_AGENTS_SUPERVISOR_MODE === "1";
  const { config } = recoveryPaths(env);
  try {
    const saved = readRecoveryConfig(config);
    if (
      saved.supervisorEnabled !== undefined &&
      typeof saved.supervisorEnabled !== "boolean"
    )
      throw new Error("The saved supervisor setting is invalid.");
    return saved.supervisorEnabled === true;
  } catch (caughterror) {
    if (!(caughterror instanceof Error)) throw caughterror;
    const error: NodeJS.ErrnoException = caughterror;
    if (error.code === "ENOENT") return false;
    throw error;
  }
}
function xml(value: unknown) {
  return String(value).replace(
    /[&<>"']/g,
    (char) =>
      (
        {
          "&": "&amp;",
          "<": "&lt;",
          ">": "&gt;",
          '"': "&quot;",
          "'": "&apos;",
        } as Record<string, string>
      )[char] ?? char,
  );
}
function launchAgent({
  label,
  python,
  supervisor,
  config,
  state,
}: Pick<ReturnType<typeof recoveryPaths>, "label" | "config" | "state"> & {
  python: string;
  supervisor: string;
}) {
  // HTTP requests and native pipes need app resource limits. Adaptive needs XPC.
  const args = [python, "-B", supervisor, "--config", config];
  return `<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>
<key>Label</key><string>${xml(label)}</string>
<key>ProgramArguments</key><array>${args.map((arg) => `<string>${xml(arg)}</string>`).join("")}</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ProcessType</key><string>Interactive</string>
<key>ThrottleInterval</key><integer>10</integer>
<key>LimitLoadToSessionType</key><string>Aqua</string>
<key>AbandonProcessGroup</key><true/>
<key>StandardOutPath</key><string>${xml(path.join(state, "background-recovery.log"))}</string>
<key>StandardErrorPath</key><string>${xml(path.join(state, "background-recovery.log"))}</string>
</dict></plist>\n`;
}
function supervisorLaunchAgent({
  label,
  supervisor,
  state,
}: {
  label: string;
  supervisor: string;
  state: string;
}) {
  const args = [supervisor, "--state", state, "--wait-for-lease"];
  return `<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>
<key>Label</key><string>${xml(label)}</string>
<key>ProgramArguments</key><array>${args.map((arg) => `<string>${xml(arg)}</string>`).join("")}</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ProcessType</key><string>Interactive</string>
<key>ThrottleInterval</key><integer>10</integer>
<key>LimitLoadToSessionType</key><string>Aqua</string>
<key>AbandonProcessGroup</key><true/>
<key>StandardOutPath</key><string>${xml(path.join(state, "supervisor.log"))}</string>
<key>StandardErrorPath</key><string>${xml(path.join(state, "supervisor.log"))}</string>
</dict></plist>\n`;
}
async function assertRecoveryBootoutSafe({
  serviceInfo,
  files,
  python,
  supervisor,
  run,
}: {
  serviceInfo: Awaited<ReturnType<RecoveryRun>>;
  files: ReturnType<typeof recoveryPaths>;
  python: string;
  supervisor: string;
  run: RecoveryRun;
}) {
  const serviceOutput =
    typeof serviceInfo?.stdout === "string" ? serviceInfo.stdout : "";
  const jobPid = Number(serviceOutput.match(/^\s*pid\s*=\s*(\d+)\s*$/m)?.[1]);
  if (!Number.isSafeInteger(jobPid) || jobPid <= 0) return;

  let owner: unknown;
  try {
    owner = JSON.parse(
      fs.readFileSync(path.join(files.state, "supervisor.lock"), "utf8"),
    );
  } catch (caughterror) {
    if (!(caughterror instanceof Error)) throw caughterror;
    const error: NodeJS.ErrnoException = caughterror;
    if (error.code === "ENOENT") return;
    throw new Error(
      `Cannot verify the supervisor owner before stopping recovery: ${error.message}`,
    );
  }
  if (
    !owner ||
    typeof owner !== "object" ||
    !("pid" in owner) ||
    typeof owner.pid !== "number" ||
    !("startTime" in owner) ||
    !Number.isSafeInteger(owner.pid) ||
    owner.pid <= 0 ||
    typeof owner.startTime !== "string"
  )
    throw new Error(
      "Cannot verify the supervisor identity before stopping recovery.",
    );
  let parentOutput;
  try {
    const result = await run("/bin/ps", [
      "-p",
      String(owner.pid),
      "-o",
      "ppid=",
    ]);
    parentOutput = typeof result?.stdout === "string" ? result.stdout : "";
  } catch (caughterror) {
    if (!(caughterror instanceof Error)) throw caughterror;
    const error: NodeJS.ErrnoException = caughterror;
    if (
      "code" in caughterror &&
      caughterror.code === 1 &&
      !("stdout" in error && String(error.stdout || "").trim())
    )
      return;
    throw new Error(
      `Cannot verify whether the supervisor belongs to recovery: ${error.message}`,
    );
  }
  if (!parentOutput.trim()) return;
  if (!Number.isSafeInteger(Number(parentOutput.trim())))
    throw new Error(
      "Cannot verify the supervisor parent process before stopping recovery.",
    );
  if (Number(parentOutput.trim()) !== jobPid) return;

  let health: unknown;
  try {
    const result = await run(python, [
      "-B",
      path.resolve(supervisor),
      "--supervisor-health",
      "--state",
      files.state,
    ]);
    health = JSON.parse(result?.stdout || "");
  } catch (caughterror) {
    if (!(caughterror instanceof Error)) throw caughterror;
    const error: NodeJS.ErrnoException = caughterror;
    throw new Error(
      `Cannot safely stop the recovery LaunchAgent while its supervisor is live: ${error.message}`,
    );
  }
  if (
    !health ||
    typeof health !== "object" ||
    !("handles" in health) ||
    !Array.isArray(health.handles)
  )
    throw new Error(
      "Cannot safely stop the recovery LaunchAgent while its supervisor status is unavailable.",
    );
  if (health.handles.length > 0)
    throw new Error(
      `Cannot stop the recovery LaunchAgent while supervisor pid ${owner.pid} owns ${health.handles.length} live handle(s). Wait for the owner to exit or all handles to close.`,
    );
}
async function ensureSupervisorAgent({
  files,
  binary,
  uid,
  run,
}: {
  files: ReturnType<typeof recoveryPaths>;
  binary: string;
  uid: number;
  run: RecoveryRun;
}) {
  fs.mkdirSync(path.dirname(files.supervisorPlist), { recursive: true });
  const nextPlist = supervisorLaunchAgent({
    label: files.supervisorLabel,
    supervisor: binary,
    state: files.state,
  });
  const temporary = `${files.supervisorPlist}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, nextPlist, { mode: 0o600 });
  fs.renameSync(temporary, files.supervisorPlist);

  const domain = `gui/${uid}`;
  const service = `${domain}/${files.supervisorLabel}`;
  let registered = true;
  try {
    await run("/bin/launchctl", ["print", service]);
  } catch {
    registered = false;
  }
  // Never unload or kickstart the owner: its child pipes carry live work.
  if (!registered)
    await run("/bin/launchctl", ["bootstrap", domain, files.supervisorPlist]);
  await run("/bin/launchctl", ["print", service]);
}
function isInstalledApplication(applicationPath: string) {
  return (
    typeof applicationPath === "string" &&
    [
      installedApplicationPath,
      "/Applications/Codex Studio.app/Contents/MacOS/studio-native-client",
    ].includes(path.resolve(applicationPath))
  );
}
function recoveryStatusLabel(enabled: boolean, available = true) {
  if (!available) return "Background recovery: Unavailable";
  return `Background recovery: ${enabled ? "On" : "Off"}`;
}
async function configureRecovery({
  applicationPath,
  resources,
  supervisor,
  enabled,
  port = 4620,
  env = process.env,
  restartEnvironment,
  home = os.homedir(),
  uid = process.getuid?.() ??
    (() => {
      throw new Error("Process identity is unavailable.");
    })(),
  run = runFile,
  checkInterpreter = checkRecoveryInterpreter,
}: RecoveryOptions) {
  if (!Number.isInteger(port) || port < 1024 || port > 65535)
    throw new Error("Invalid recovery port.");
  if (!isInstalledApplication(applicationPath))
    throw new Error(
      "Background recovery can be registered only by /Applications/Codex Studio.app.",
    );
  const files = recoveryPaths(env, home);
  const domain = `gui/${uid}`;
  const service = `${domain}/${files.label}`;
  const authoritative =
    restartEnvironment && typeof restartEnvironment === "object";
  let saved: SavedRecovery = {};
  try {
    saved = readRecoveryConfig(files.config);
  } catch (caughterror) {
    if (!(caughterror instanceof Error)) throw caughterror;
    const error: NodeJS.ErrnoException = caughterror;
    if (error.code !== "ENOENT") throw error;
  }
  if (
    saved.supervisorEnabled !== undefined &&
    typeof saved.supervisorEnabled !== "boolean"
  )
    throw new Error("The saved supervisor setting is invalid.");
  const launchEnv = savedLaunchEnvironment(env, saved, files.state);
  if (authoritative) {
    for (const key of restartKeys) {
      delete launchEnv[key];
      if (typeof restartEnvironment[key] === "string")
        launchEnv[key] = restartEnvironment[key];
    }
  }
  const requestedSupervisorMode = authoritative
    ? restartEnvironment.CODEX_AGENTS_SUPERVISOR_MODE
    : env.CODEX_AGENTS_SUPERVISOR_MODE;
  const supervisorEnabled =
    requestedSupervisorMode === undefined
      ? saved.supervisorEnabled === true
      : requestedSupervisorMode === "1";
  // Python runs only the desktop helpers; backend binaries have their own owners.
  const interpreterEnv: NodeJS.ProcessEnv = { ...launchEnv, PATH: env.PATH };
  delete interpreterEnv.CODEX_AGENTS_PYTHON;
  // Turning recovery off never needs a supported interpreter.
  const python = enabled
    ? await supportedPython({
        env: interpreterEnv,
        saved,
        script: supervisor,
        checkInterpreter,
      })
    : executable("python3", interpreterEnv);
  const rustSelection: {
    backend?: string;
    runtimeServer?: string;
    processSupervisor?: string;
  } =
    enabled || supervisorEnabled
      ? {
          backend: "rust",
          runtimeServer: runtimeServer(resources, {
            ...launchEnv,
            CODEX_RUNTIME_SERVER:
              env.CODEX_RUNTIME_SERVER || saved.runtimeServer,
          }),
          ...(supervisorEnabled
            ? {
                processSupervisor: processSupervisor(resources, {
                  ...env,
                  CODEX_PROCESS_SUPERVISOR:
                    env.CODEX_PROCESS_SUPERVISOR || saved.processSupervisor,
                }),
              }
            : {}),
        }
      : {};
  if (supervisorEnabled && saved.backend !== "rust")
    await ensureSupervisorAgent({
      files,
      binary: rustSelection.processSupervisor ?? "",
      uid,
      run,
    });
  if (!enabled) {
    let serviceInfo;
    try {
      serviceInfo = await run("/bin/launchctl", ["print", service]);
    } catch {
      atomicJSON(files.config, {
        ...saved,
        version: 1,
        stateDir: files.state,
        enabled: false,
        supervisorEnabled,
      });
      fs.rmSync(files.plist, { force: true });
      return { enabled: false, ...files };
    }
    await assertRecoveryBootoutSafe({
      serviceInfo,
      files,
      python,
      supervisor,
      run,
    });
    atomicJSON(files.config, {
      ...saved,
      version: 1,
      stateDir: files.state,
      enabled: false,
      supervisorEnabled,
    });
    await run("/bin/launchctl", ["bootout", service]);
    fs.rmSync(files.plist, { force: true });
    return { enabled: false, ...files };
  }
  const codex = executable("codex", launchEnv);
  for (const filename of [
    supervisor,
    rustSelection.runtimeServer,
    rendererIndex(resources),
  ])
    fs.accessSync(filename ?? "", fs.constants.R_OK);
  const environment: NodeJS.ProcessEnv = {};
  for (const key of restartKeys)
    if (key !== "CODEX_AGENTS_SUPERVISOR_MODE" && launchEnv[key] !== undefined)
      environment[key] = launchEnv[key];
  const launchPath = launchEnv.PATH || "/usr/bin:/bin";
  environment.PATH =
    launchPath.split(path.delimiter)[0] === path.dirname(codex)
      ? launchPath
      : `${path.dirname(codex)}:${launchPath}`;
  const config = {
    version: 1,
    enabled: true,
    supervisorEnabled,
    stateDir: files.state,
    resources: path.resolve(resources),
    python,
    codex,
    port,
    environment,
    unsetEnvironment: restartKeys.filter(
      (key) => environment[key] === undefined,
    ),
    ...rustSelection,
  };
  let previousPlist;
  try {
    previousPlist = fs.readFileSync(files.plist, "utf8");
  } catch (caughterror) {
    if (!(caughterror instanceof Error)) throw caughterror;
    const error: NodeJS.ErrnoException = caughterror;
    if (error.code !== "ENOENT") throw error;
  }
  const nextPlist = launchAgent({
    ...files,
    python,
    supervisor: path.resolve(supervisor),
  });
  let serviceInfo;
  let registered = true;
  try {
    serviceInfo = await run("/bin/launchctl", ["print", service]);
  } catch {
    registered = false;
  }
  // Save automatic interpreter and policy changes for the next registration.
  // The registered recovery process can keep its original interpreter only while that
  // interpreter runs the script; otherwise launchd must load the new arguments.
  const retainedPythonPlist =
    typeof saved.python === "string" &&
    path.isAbsolute(saved.python) &&
    (saved.python === python ||
      (await interpreterRuns(checkInterpreter, saved.python, supervisor)))
      ? launchAgent({
          ...files,
          python: saved.python,
          supervisor: path.resolve(supervisor),
        })
      : nextPlist;
  const compatiblePlists = [nextPlist, retainedPythonPlist].flatMap((plist) => [
    plist,
    plist.replace("<key>ProcessType</key><string>Interactive</string>\n", ""),
  ]);
  const restartRecovery =
    registered &&
    previousPlist !== undefined &&
    !compatiblePlists.includes(previousPlist);
  if (restartRecovery)
    await assertRecoveryBootoutSafe({
      serviceInfo,
      files,
      python,
      supervisor,
      run,
    });
  atomicJSON(files.config, config);
  fs.mkdirSync(path.dirname(files.plist), { recursive: true });
  const temporary = `${files.plist}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, nextPlist, { mode: 0o600 });
  fs.renameSync(temporary, files.plist);
  if (restartRecovery) {
    // Restart only recovery after proving it does not own live supervisor work.
    await run("/bin/launchctl", ["bootout", service]);
    registered = false;
  }
  if (!registered) {
    try {
      await run("/bin/launchctl", ["bootstrap", domain, files.plist]);
    } catch (caughterror) {
      if (!(caughterror instanceof Error)) throw caughterror;
      const error: NodeJS.ErrnoException = caughterror;
      try {
        // A failed response does not prove that registration failed.
        await run("/bin/launchctl", ["print", service]);
      } catch (caughtstatusError) {
        if (!(caughtstatusError instanceof Error)) throw caughtstatusError;
        if (
          !restartRecovery ||
          !("code" in caughtstatusError && caughtstatusError.code === 113)
        )
          throw error;
        fs.writeFileSync(temporary, previousPlist ?? "", { mode: 0o600 });
        fs.renameSync(temporary, files.plist);
        try {
          await run("/bin/launchctl", ["bootstrap", domain, files.plist]);
          await run("/bin/launchctl", ["print", service]);
        } catch (caughtrestoreError) {
          if (!(caughtrestoreError instanceof Error)) throw caughtrestoreError;
          const restoreError: NodeJS.ErrnoException = caughtrestoreError;
          throw new Error(
            `Cannot update or restore background recovery: ${restoreError.message}`,
            { cause: error },
          );
        }
        throw new Error(
          "Cannot update background recovery. The previous recovery service was restored.",
          { cause: error },
        );
      }
    }
  }
  // Read back launchd registration. Writing a plist alone does not install a service.
  await run("/bin/launchctl", ["print", service]);
  return { enabled: true, ...files };
}
export {
  atomicJSON,
  configureRecovery,
  recoveryPreference,
  supervisorPreference,
  recoveryPaths,
  launchAgent,
  supervisorLaunchAgent,
  isInstalledApplication,
  recoveryStatusLabel,
};

export function trackDesktopRecovery({
  app,
  env = process.env,
}: {
  app: Pick<App, "getPath"> & Pick<NodeJS.EventEmitter, "on">;
  env?: NodeJS.ProcessEnv;
}) {
  const { state } = recoveryPaths(env);
  const filename = path.join(state, "desktop-recovery.json");
  const startedAt = execFileSync(
    "/bin/ps",
    ["-p", String(process.pid), "-o", "lstart="],
    { encoding: "utf8", env: { ...process.env, LC_ALL: "C" } },
  ).trim();
  if (!startedAt) throw new Error("Cannot identify the desktop process.");
  const intent = {
    version: 1,
    desiredOpen: true,
    pid: process.pid,
    startedAt,
    executable: process.execPath,
    profile: app.getPath("userData"),
  };
  atomicJSON(filename, intent);
  const closeExplicitly = () => {
    try {
      atomicJSON(filename, { ...intent, desiredOpen: false });
    } catch (caughterror) {
      if (!(caughterror instanceof Error)) throw caughterror;
      const error: NodeJS.ErrnoException = caughterror;
      // A missing intent is safer than reopening after an unsaved explicit Quit.
      try {
        fs.unlinkSync(filename);
      } catch {}
      console.error("Cannot save desktop close intent:", error.message);
    }
  };
  // Electron does not identify every macOS Quit reason. A graceful exit always
  // withdraws the open request; macOS owns its normal reopen-windows preference.
  app.on("before-quit", closeExplicitly);
  app.on("window-all-closed", closeExplicitly);
  return { filename, closeExplicitly, windowClosing: closeExplicitly };
}
