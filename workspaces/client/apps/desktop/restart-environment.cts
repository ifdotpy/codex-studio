// Owner of persisted launch-environment version and allowed keys; Rust parity is checked by backend.test.mjs.
const restartEnvironmentVersion = 1;
const restartKeys = [
  "CODEX_HOME",
  "CODEX_CANVAS_CWD",
  "CODEX_CANVAS_CONCURRENCY",
  "CODEX_BIN",
  "SHELL",
  "LANG",
  "LC_ALL",
  "CODEX_AGENTS_SUPERVISOR_MODE",
  "CODEX_AGENTS_SUPERVISOR_FALLBACK",
];
// Names that the deleted Python backend saved and no runtime reads any more.
// A saved record may still name them in `environment` or `unsetEnvironment`:
// they are accepted, never restored, and removed from the launch environment.
const retiredRestartKeys = ["CODEX_AGENTS_PYTHON"];
const environmentKeys = new Set([...restartKeys, "PATH"]);

function savedLaunchEnvironment(env, saved, state) {
  const result = { ...env };
  // Old preference-only files do not describe a backend launch environment.
  if (saved.environment === undefined && saved.unsetEnvironment === undefined)
    return result;
  const environment = saved.environment;
  const unset = saved.unsetEnvironment;
  if (
    saved.version !== restartEnvironmentVersion ||
    saved.stateDir !== state ||
    !environment ||
    typeof environment !== "object" ||
    Array.isArray(environment) ||
    !Array.isArray(unset) ||
    Object.entries(environment).some(
      ([key, value]) =>
        !(environmentKeys.has(key) || retiredRestartKeys.includes(key)) ||
        typeof value !== "string",
    ) ||
    unset.some(
      (key) =>
        !(restartKeys.includes(key) || retiredRestartKeys.includes(key)) ||
        Object.hasOwn(environment, key),
    )
  )
    throw new Error(
      "The saved restart environment is invalid or uses a different state directory.",
    );
  for (const key of unset) delete result[key];
  Object.assign(result, environment);
  for (const key of retiredRestartKeys) delete result[key];
  return result;
}

module.exports = {
  restartKeys,
  retiredRestartKeys,
  restartEnvironmentVersion,
  savedLaunchEnvironment,
};
