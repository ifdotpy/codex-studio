// Native session files move without credentials or a model request.
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const run = promisify(execFile);
export const movePrompt = { snapshot: true, excludeDynamicSections: true };

export async function moveIdentity(executable) {
  const { stdout } = await run(executable, ["auth", "status", "--json"], {
    timeout: 10000,
    maxBuffer: 4096,
  });
  const account = JSON.parse(stdout);
  if (
    !account.loggedIn ||
    account.authMethod !== "claude.ai" ||
    typeof account.email !== "string" ||
    !account.email ||
    typeof account.orgId !== "string" ||
    !account.orgId
  )
    throw new Error(
      "The Claude account and organization are unavailable. Sign in with the target subscription organization",
    );
  return { email: account.email, organizationId: account.orgId };
}

export async function moveVersions(executable) {
  const { stdout } = await run(executable, ["--version"], {
    timeout: 10000,
    maxBuffer: 4096,
  });
  const cli = stdout.trim().match(/^([\d.]+)/)?.[1];
  if (!cli) throw new Error("The Claude CLI version is unavailable");
  const sdk = JSON.parse(
    await fs.readFile(
      path.join(
        path.dirname(require.resolve("@anthropic-ai/claude-agent-sdk")),
        "package.json",
      ),
      "utf8",
    ),
  ).version;
  return { cli, sdk };
}
export async function nativeFile(
  sessionId,
  configDir = process.env.CLAUDE_CONFIG_DIR ||
    path.join(os.homedir(), ".claude"),
  cwd,
) {
  if (!/^[a-f0-9-]{36}$/i.test(sessionId))
    throw new Error("Invalid Claude session identity");
  const projects = path.join(configDir, "projects");
  const found = [];
  for (const entry of await fs.readdir(projects, { withFileTypes: true })) {
    if (!entry.isDirectory() || entry.isSymbolicLink()) continue;
    const file = path.join(projects, entry.name, sessionId + ".jsonl");
    try {
      const stat = await fs.lstat(file);
      if (stat.isFile() && !stat.isSymbolicLink()) found.push(file);
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
  }
  const current = cwd && nativeDestination(sessionId, cwd, configDir);
  if (current && found.includes(current)) return current;
  if (found.length !== 1)
    throw new Error(
      "The complete Claude native history is missing or ambiguous",
    );
  return found[0];
}
export function nativeDestination(
  sessionId,
  cwd,
  configDir = process.env.CLAUDE_CONFIG_DIR ||
    path.join(os.homedir(), ".claude"),
) {
  if (!/^[a-f0-9-]{36}$/i.test(sessionId) || !path.isAbsolute(cwd))
    throw new Error("Invalid Claude move destination");
  return path.join(
    configDir,
    "projects",
    cwd.replace(/[^a-zA-Z0-9]/g, "-"),
    sessionId + ".jsonl",
  );
}
export async function publishNative(
  source,
  destination,
  allowExtension = false,
) {
  const bytes = await fs.readFile(source);
  if (bytes.length > 256 * 1024 * 1024 || !bytes.length || bytes.at(-1) !== 10)
    throw new Error(
      "The complete Claude history exceeds its limit or has an incomplete record",
    );
  await fs.mkdir(path.dirname(destination), { recursive: true, mode: 0o700 });
  try {
    const old = await fs.readFile(destination);
    if (old.equals(bytes)) return;
    if (
      !allowExtension ||
      old.length > bytes.length ||
      !bytes.subarray(0, old.length).equals(old)
    )
      throw new Error("The destination Claude history has different bytes");
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  const temporary = destination + ".move-" + process.pid;
  const handle = await fs.open(temporary, "wx", 0o600);
  try {
    await handle.writeFile(bytes);
    await handle.sync();
  } finally {
    await handle.close();
  }
  await fs.rename(temporary, destination);
}
