// Native session files move without credentials or a model request.
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { createRequire } from "node:module";
import { createHash } from "node:crypto";
const require = createRequire(import.meta.url);
const run = promisify(execFile);
export const movePrompt = { snapshot: true, excludeDynamicSections: true };

export async function savedPromptProof(file, session) {
  const bytes = await fs.readFile(file);
  if (bytes.length > 256 * 1024 * 1024 || bytes.at(-1) !== 10)
    throw new Error("The complete native snapshot history is unavailable");
  let snapshot;
  for (const line of bytes.toString("utf8").trimEnd().split("\n")) {
    const row = JSON.parse(line);
    if (row.sessionId && row.sessionId !== (session.nativeId || session.id))
      throw new Error("The native snapshot session identity differs");
    if (row.type === "attachment" && row.attachment?.type === "prompt_snapshot")
      snapshot = row.attachment;
  }
  if (
    !Array.isArray(snapshot?.systemPrompt) ||
    !snapshot.systemPrompt.length ||
    !snapshot.systemPrompt.every((text) => typeof text === "string") ||
    !Array.isArray(snapshot.tools) ||
    !snapshot.tools.every(
      (entry) =>
        typeof entry.name === "string" &&
        entry.input_schema &&
        typeof entry.input_schema === "object",
    )
  )
    throw new Error(
      "The session has no verified saved prompt snapshot with complete ordered tool schemas",
    );
  return {
    snapshotHash: createHash("sha256")
      .update(JSON.stringify(snapshot))
      .digest("hex"),
    tools: snapshot.tools,
    session: Object.fromEntries(
      [
        "model",
        "dynamicTools",
        "developerInstructions",
        "approvalPolicy",
        "sandbox",
        "claude",
        "moveTurnOptions",
      ]
        .filter((key) => session[key] !== undefined)
        .map((key) => [key, structuredClone(session[key])]),
    ),
  };
}

export function verifyToolProof(proof, studio, offered) {
  const builtin = proof.tools.filter(
    (entry) => !entry.name.startsWith("mcp__"),
  );
  const mcp = proof.tools.filter((entry) => entry.name.startsWith("mcp__"));
  if (mcp.some((entry) => !entry.name.startsWith("mcp__studio__")))
    throw new Error(
      "External MCP tool snapshots are not supported yet. Their target schemas cannot be verified without a server catalog proof",
    );
  const shape = (entry) => ({
    name: entry.name,
    description: entry.description || "",
    input_schema: entry.input_schema,
  });
  if (JSON.stringify(mcp.map(shape)) !== JSON.stringify(studio.map(shape)))
    throw new Error(
      "The effective target Studio tool definitions differ in names, schemas, or order",
    );
  if (offered && builtin.some((entry) => !offered.includes(entry.name)))
    throw new Error("The target CLI does not offer a saved builtin tool");
  return builtin.map((entry) => entry.name);
}

export async function studioToolCatalog(server, expectedToolCount) {
  const { Client } = await import("@modelcontextprotocol/sdk/client/index.js");
  const { InMemoryTransport } =
    await import("@modelcontextprotocol/sdk/inMemory.js");
  const [left, right] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: "studio-move-proof", version: "1" });
  try {
    await server.instance.connect(right);
    await client.connect(left);
    if (!client.getServerCapabilities()?.tools) return [];
    let result;
    try {
      result = await client.listTools();
    } catch (error) {
      // The SDK advertises tools for an empty server but registers no handler.
      // Only Studio's known empty definition list permits this exact rejection.
      if (expectedToolCount === 0 && error.code === -32601) return [];
      throw error;
    }
    if (result.nextCursor)
      throw new Error("The complete Studio tool catalog is unavailable");
    return result.tools.map((entry) => ({
      name: "mcp__studio__" + entry.name,
      description: entry.description || "",
      input_schema: entry.inputSchema,
    }));
  } finally {
    await client.close();
    await server.instance.close();
  }
}

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
