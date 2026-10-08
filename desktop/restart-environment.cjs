const restartKeys = [
  "CODEX_HOME",
  "CODEX_CANVAS_CWD",
  "CODEX_CANVAS_CONCURRENCY",
  "CODEX_BIN",
  "CODEX_AGENTS_PYTHON",
  "SHELL",
  "LANG",
  "LC_ALL",
  "CODEX_AGENTS_SUPERVISOR_MODE",
  "CODEX_AGENTS_SUPERVISOR_FALLBACK",
];
const environmentKeys = new Set([...restartKeys, "PATH"]);

function savedLaunchEnvironment(env, saved, state) {
  const result = { ...env };
  // Old preference-only files do not describe a backend launch environment.
  if (saved.environment === undefined && saved.unsetEnvironment === undefined)
    return result;
  const environment = saved.environment;
  const unset = saved.unsetEnvironment;
  if (
    saved.version !== 1 ||
    saved.stateDir !== state ||
    !environment ||
    typeof environment !== "object" ||
    Array.isArray(environment) ||
    !Array.isArray(unset) ||
    Object.entries(environment).some(
      ([key, value]) => !environmentKeys.has(key) || typeof value !== "string",
    ) ||
    unset.some(
      (key) => !restartKeys.includes(key) || Object.hasOwn(environment, key),
    )
  )
    throw new Error(
      "The saved restart environment is invalid or uses a different state directory.",
    );
  for (const key of unset) delete result[key];
  return Object.assign(result, environment);
}

module.exports = { restartKeys, savedLaunchEnvironment };
