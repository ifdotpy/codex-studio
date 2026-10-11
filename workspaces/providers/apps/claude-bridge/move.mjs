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

const toolSchema = (entry) => entry?.input_schema ?? entry?.schema;
const completeTool = (entry) =>
  typeof entry?.name === "string" &&
  entry.name.length > 0 &&
  toolSchema(entry) !== null &&
  typeof toolSchema(entry) === "object" &&
  !Array.isArray(toolSchema(entry));
const normalizeTool = ({ schema, ...entry }) => ({
  ...entry,
  input_schema: entry.input_schema ?? schema,
});

// The paired channel signs canonical JSON and so sorts object keys, while the order of a tool
// schema's properties is part of the prompt. A move carries the source's tool lists as exact
// JSON text too; the target reads that text, after checking it against the sorted copy.
const canonicalJson = (value) =>
  JSON.stringify(value, (key, entry) =>
    entry && typeof entry === "object" && !Array.isArray(entry)
      ? Object.fromEntries(
          Object.keys(entry)
            .sort()
            .map((name) => [name, entry[name]]),
        )
      : entry,
  );
export function exactList(sorted, text, what) {
  if (typeof text !== "string") return sorted;
  const exact = JSON.parse(text);
  if (sorted !== undefined && canonicalJson(exact) !== canonicalJson(sorted))
    throw new Error(`The exact ${what} differ from the moved copy`);
  return exact;
}
export function exactSession(session) {
  if (typeof session?.dynamicToolsJson !== "string") return session;
  return {
    ...session,
    dynamicTools: exactList(
      session.dynamicTools,
      session.dynamicToolsJson,
      "Studio tools",
    ),
  };
}
export function exactProof(proof) {
  if (typeof proof?.compiledToolsJson !== "string") return proof;
  return {
    ...proof,
    compiledTools: exactList(
      proof.compiledTools,
      proof.compiledToolsJson,
      "compiled Studio tools",
    ),
  };
}

export async function savedPromptProof(file, session) {
  const bytes = await fs.readFile(file);
  if (bytes.length > 256 * 1024 * 1024 || bytes.at(-1) !== 10)
    throw new Error("The complete native snapshot history is unavailable");
  let snapshot;
  const deferred = new Map();
  const activeNames = new Set();
  // Every name a delta announced since the last compaction. Connectors drop and add their
  // names around each turn, so the active set at one moment differs from the next.
  const announced = new Set();
  for (const line of bytes.toString("utf8").trimEnd().split("\n")) {
    const row = JSON.parse(line);
    if (row.sessionId && row.sessionId !== (session.nativeId || session.id))
      throw new Error("The native snapshot session identity differs");
    if (row.type === "system" && row.subtype === "compact_boundary") {
      snapshot = undefined;
      deferred.clear();
      activeNames.clear();
      announced.clear();
    }
    if (row.type === "attachment" && row.attachment?.type === "prompt_snapshot")
      snapshot = row.attachment;
    if (
      row.type === "attachment" &&
      row.attachment?.type === "deferred_tools_record"
    ) {
      if (!Array.isArray(row.attachment.entries))
        throw new Error("The saved deferred tool schemas are incomplete");
      for (const entry of row.attachment.entries) {
        if (!completeTool(entry))
          throw new Error("The saved deferred tool schemas are incomplete");
        deferred.set(entry.name, entry);
        activeNames.add(entry.name);
      }
    }
    if (
      row.type === "attachment" &&
      row.attachment?.type === "deferred_tools_delta"
    ) {
      // Newer CLIs put the definitions that a delta surfaces into the delta itself; an
      // entry with sameAs repeats a definition that an earlier delta gave.
      const surfaced = row.attachment.surfacedDefinitions;
      if (surfaced !== undefined && !Array.isArray(surfaced))
        throw new Error("The saved deferred tool schemas are incomplete");
      for (const entry of surfaced || []) {
        if (entry?.definition === undefined) continue;
        if (!completeTool(entry.definition))
          throw new Error("The saved deferred tool schemas are incomplete");
        deferred.set(entry.definition.name, entry.definition);
      }
      for (const key of ["removedNames", "addedNames"])
        if (
          row.attachment[key] !== undefined &&
          (!Array.isArray(row.attachment[key]) ||
            !row.attachment[key].every(
              (name) => typeof name === "string" && name,
            ))
        )
          throw new Error("The saved deferred tool names are incomplete");
      for (const name of row.attachment.removedNames || [])
        activeNames.delete(name);
      for (const name of row.attachment.addedNames || []) {
        activeNames.add(name);
        announced.add(name);
      }
    }
  }
  if (
    !Array.isArray(snapshot?.systemPrompt) ||
    !snapshot.systemPrompt.length ||
    !snapshot.systemPrompt.every((text) => typeof text === "string") ||
    !Array.isArray(snapshot.tools) ||
    !snapshot.tools.every(completeTool)
  )
    throw new Error(
      "The session has no verified saved prompt snapshot with complete ordered tool schemas",
    );
  // Newer CLIs mark inlineTools true, yet keep every MCP tool out of the snapshot and deliver
  // it through deferred records and deltas, as inlineTools false does. Such a snapshot gets the
  // deferred proof; its external servers are those whose definitions the history holds.
  const deferredOnly =
    snapshot.inlineTools !== false &&
    !snapshot.tools.some((entry) => entry.name.startsWith("mcp__")) &&
    (deferred.size > 0 || [...activeNames].some((n) => n.startsWith("mcp__")));
  return {
    snapshotHash: createHash("sha256")
      .update(JSON.stringify(snapshot))
      .digest("hex"),
    tools: snapshot.tools.map(normalizeTool),
    ...(snapshot.inlineTools === false || deferredOnly
      ? {
          deferredTools: [...deferred.values()],
          activeDeferredNames: [...activeNames],
          deferredBuiltinNames: [...announced].filter(
            (name) => !name.startsWith("mcp__"),
          ),
          inlineTools: false,
          ...(deferredOnly ? { surfacedExternal: true } : {}),
        }
      : {}),
    session: {
      ...Object.fromEntries(
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
      ...(session.dynamicTools !== undefined
        ? { dynamicToolsJson: JSON.stringify(session.dynamicTools) }
        : {}),
    },
  };
}

export function verifyToolProof(proof, studio, offered) {
  const safeNames = (entries) =>
    entries.map((entry) =>
      /^[A-Za-z0-9_.:+ -]{1,128}$/.test(entry.name)
        ? entry.name
        : "<unavailable>",
    );
  const names = (entries) => safeNames(entries).join(", ") || "(none)";
  const refusal = (message, kind, source, target = [], changed = []) =>
    Object.assign(new Error(message), {
      data: {
        moveRefusal: {
          kind,
          sourceNames: safeNames(source),
          targetNames: safeNames(target),
          changedNames: safeNames(changed),
        },
      },
    });
  const builtin = proof.tools.filter(
    (entry) => !entry.name.startsWith("mcp__"),
  );
  if (proof.inlineTools === false)
    for (const name of proof.deferredBuiltinNames ||
      proof.activeDeferredNames ||
      [])
      if (
        !name.startsWith("mcp__") &&
        !builtin.some((entry) => entry.name === name)
      )
        builtin.push({ name });
  // Deferred schemas remain transcript input. Only inline schemas define the
  // frozen API tool prefix. Compare the current compiled catalog separately.
  const mcp =
    proof.inlineTools === false
      ? proof.compiledTools || studio
      : proof.tools.filter((entry) => entry.name.startsWith("mcp__"));
  if (mcp.some((entry) => !entry.name.startsWith("mcp__studio__")))
    throw refusal(
      "External MCP tool snapshots are not supported yet. Source tools [" +
        names(mcp.filter((entry) => !entry.name.startsWith("mcp__studio__"))) +
        "]. Their target schemas cannot be verified without a server catalog proof",
      "external_tools",
      mcp.filter((entry) => !entry.name.startsWith("mcp__studio__")),
    );
  // A source catalog that crossed the paired channel has its object keys sorted (the channel
  // signs canonical JSON), so definitions compare by content, with the tool order kept.
  const canonical = (value) =>
    Array.isArray(value)
      ? value.map(canonical)
      : value && typeof value === "object"
        ? Object.fromEntries(
            Object.keys(value)
              .sort()
              .map((key) => [key, canonical(value[key])]),
          )
        : value;
  const shape = (entry) =>
    JSON.stringify(
      canonical({
        name: entry.name,
        description: entry.description || "",
        input_schema: entry.input_schema,
      }),
    );
  if (JSON.stringify(mcp.map(shape)) !== JSON.stringify(studio.map(shape))) {
    const before = new Map(mcp.map((entry) => [entry.name, shape(entry)]));
    const after = new Map(studio.map((entry) => [entry.name, shape(entry)]));
    const changed = mcp.filter(
      (entry) =>
        after.has(entry.name) &&
        before.get(entry.name) !== after.get(entry.name),
    );
    throw refusal(
      "The effective target Studio tool definitions differ in names, schemas, or order. Source tools [" +
        names(mcp) +
        "]; target tools [" +
        names(studio) +
        "]; changed definitions [" +
        names(changed) +
        "]",
      "studio_tools",
      mcp,
      studio,
      changed,
    );
  }
  if (offered && builtin.some((entry) => !offered.includes(entry.name)))
    throw refusal(
      "The target CLI does not offer saved builtin tools [" +
        names(builtin.filter((entry) => !offered.includes(entry.name))) +
        "]",
      "builtin_tools",
      builtin,
      offered.map((name) => ({ name })),
      builtin.filter((entry) => !offered.includes(entry.name)),
    );
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
