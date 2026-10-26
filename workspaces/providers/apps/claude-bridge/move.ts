import type { Session } from "./session-state.ts";
import type { McpSdkServerConfigWithInstance } from "@anthropic-ai/claude-agent-sdk";
export type ProofTool = {
  name: string;
  description?: unknown;
  input_schema?: unknown;
  schema?: unknown;
  [key: string]: unknown;
};
export type ToolProof = {
  tools?: ProofTool[];
  inlineTools?: boolean;
  surfacedExternal?: boolean;
  deferredTools?: ProofTool[];
  activeDeferredNames?: string[];
  deferredBuiltinNames?: string[];
  compiledTools?: ProofTool[];
  compiledToolsJson?: string;
};
export type MoveProof = ToolProof & {
  snapshotHash: string;
  tools: ProofTool[];
  session: Partial<Session>;
};
export const isRecord = (value: unknown): value is Record<string, unknown> =>
  value !== null && typeof value === "object" && !Array.isArray(value);
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
// The paired channel signs canonical JSON, so a catalog from the source arrives with sorted
// object keys; catalogs and option hashes compare in this canonical form.
export const canonicalJson = (value: unknown) =>
  JSON.stringify(value, (_key, entry: unknown) =>
    entry && typeof entry === "object" && !Array.isArray(entry)
      ? Object.fromEntries(
          Object.keys(entry)
            .sort()
            .map((name) => [name, Reflect.get(entry, name)]),
        )
      : entry,
  );

export const movePrompt = {
  snapshot: true,
  excludeDynamicSections: true,
} as const;

const completeTool = (entry: unknown): entry is ProofTool => {
  if (!isRecord(entry)) return false;
  const schema = entry.input_schema ?? entry.schema;
  return (
    typeof entry.name === "string" &&
    entry.name.length > 0 &&
    schema !== null &&
    typeof schema === "object" &&
    !Array.isArray(schema)
  );
};
const normalizeTool = ({ schema, ...entry }: ProofTool) => ({
  ...entry,
  input_schema: entry.input_schema ?? schema,
});

// The paired channel signs canonical JSON and so sorts object keys, while the order of a tool
// schema's properties is part of the prompt. A move carries the source's tool lists as exact
// JSON text too; the target reads that text, after checking it against the sorted copy.
// Exact-list text is written by this bridge with JSON.stringify before a signed move.
// Its sorted counterpart is checked below before it is used as the same type.
function parseExactList<T>(text: string): T {
  return JSON.parse(text);
}
export function exactList<T>(sorted: T, text: unknown, what: string): T {
  if (typeof text !== "string") return sorted;
  const exact = parseExactList<T>(text);
  if (sorted !== undefined && canonicalJson(exact) !== canonicalJson(sorted))
    throw new Error(`The exact ${what} differ from the moved copy`);
  return exact;
}
/** The server keeps records with sorted keys, so later turns of a moved session bring its
 * tools in another key order, and a schema compiled from them lists `required` in another
 * order. Equal content keeps the exact moved order; changed content stays as sent. */
export function exactTurnTools<T>(
  session: Pick<Session, "dynamicToolsJson">,
  tools: T,
): T {
  if (typeof session?.dynamicToolsJson !== "string" || !Array.isArray(tools))
    return tools;
  const exact = parseExactList<T>(session.dynamicToolsJson);
  return canonicalJson(exact) === canonicalJson(tools) ? exact : tools;
}
export function exactSession<
  T extends Pick<Session, "dynamicToolsJson" | "dynamicTools">,
>(session: T): T {
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
export function exactProof<
  T extends Pick<ToolProof, "compiledToolsJson" | "compiledTools">,
>(proof: T): T {
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

export async function savedPromptProof(
  file: string,
  session: Pick<
    Session,
    | "id"
    | "nativeId"
    | "dynamicTools"
    | "model"
    | "developerInstructions"
    | "approvalPolicy"
    | "sandbox"
    | "claude"
    | "moveTurnOptions"
  >,
) {
  const bytes = await fs.readFile(file);
  if (bytes.length > 256 * 1024 * 1024 || bytes.at(-1) !== 10)
    throw new Error("The complete native snapshot history is unavailable");
  let snapshot: Record<string, unknown> | undefined;
  const deferred = new Map<string, ProofTool>();
  const activeNames = new Set<string>();
  // Every name a delta announced since the last compaction. Connectors drop and add their
  // names around each turn, so the active set at one moment differs from the next.
  const announced = new Set<string>();
  for (const line of bytes.toString("utf8").trimEnd().split("\n")) {
    const row: unknown = JSON.parse(line);
    if (!isRecord(row))
      throw new Error("The complete native snapshot history is unavailable");
    const attachment = isRecord(row.attachment) ? row.attachment : undefined;
    if (row.sessionId && row.sessionId !== (session.nativeId || session.id))
      throw new Error("The native snapshot session identity differs");
    if (row.type === "system" && row.subtype === "compact_boundary") {
      snapshot = undefined;
      deferred.clear();
      activeNames.clear();
      announced.clear();
    }
    if (row.type === "attachment" && attachment?.type === "prompt_snapshot")
      snapshot = attachment;
    if (
      row.type === "attachment" &&
      attachment?.type === "deferred_tools_record"
    ) {
      if (!Array.isArray(attachment.entries))
        throw new Error("The saved deferred tool schemas are incomplete");
      for (const entry of attachment.entries) {
        if (!completeTool(entry))
          throw new Error("The saved deferred tool schemas are incomplete");
        deferred.set(entry.name, entry);
        activeNames.add(entry.name);
      }
    }
    if (
      row.type === "attachment" &&
      attachment?.type === "deferred_tools_delta"
    ) {
      // Newer CLIs put the definitions that a delta surfaces into the delta itself; an
      // entry with sameAs repeats a definition that an earlier delta gave.
      const surfaced: unknown = attachment.surfacedDefinitions;
      if (surfaced !== undefined && !Array.isArray(surfaced))
        throw new Error("The saved deferred tool schemas are incomplete");
      for (const entry of (surfaced || []) as unknown[]) {
        if (!isRecord(entry) || entry.definition === undefined) continue;
        if (!completeTool(entry.definition))
          throw new Error("The saved deferred tool schemas are incomplete");
        deferred.set(entry.definition.name, entry.definition);
      }
      for (const key of ["removedNames", "addedNames"] as const)
        if (
          attachment[key] !== undefined &&
          (!Array.isArray(attachment[key]) ||
            !attachment[key].every((name) => typeof name === "string" && name))
        )
          throw new Error("The saved deferred tool names are incomplete");
      for (const name of (attachment.removedNames || []) as string[])
        activeNames.delete(name);
      for (const name of (attachment.addedNames || []) as string[]) {
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
        (
          [
            "model",
            "dynamicTools",
            "developerInstructions",
            "approvalPolicy",
            "sandbox",
            "claude",
            "moveTurnOptions",
          ] as const
        )
          .filter((key) => session[key] !== undefined)
          .map((key) => [key, structuredClone(session[key])]),
      ),
      ...(session.dynamicTools !== undefined
        ? { dynamicToolsJson: JSON.stringify(session.dynamicTools) }
        : {}),
    },
  };
}

export function verifyToolProof(
  proof: ToolProof & { tools: ProofTool[] },
  studio: ProofTool[],
  offered?: string[],
) {
  const safeNames = (entries: ProofTool[]) =>
    entries.map((entry) =>
      /^[A-Za-z0-9_.:+ -]{1,128}$/.test(entry.name)
        ? entry.name
        : "<unavailable>",
    );
  const names = (entries: ProofTool[]) =>
    safeNames(entries).join(", ") || "(none)";
  const refusal = (
    message: string,
    kind: string,
    source: ProofTool[],
    target: ProofTool[] = [],
    changed: ProofTool[] = [],
  ) =>
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
  const shape = (entry: ProofTool) =>
    canonicalJson({
      name: entry.name,
      description: entry.description || "",
      input_schema: entry.input_schema,
    });
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

export async function studioToolCatalog(
  server: McpSdkServerConfigWithInstance,
  expectedToolCount?: number,
) {
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
      if (expectedToolCount === 0 && isRecord(error) && error.code === -32601)
        return [];
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

export async function moveIdentity(executable: string) {
  const { stdout } = await run(executable, ["auth", "status", "--json"], {
    timeout: 10000,
    maxBuffer: 4096,
  });
  const account: unknown = JSON.parse(stdout);
  if (
    !isRecord(account) ||
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

export async function moveVersions(executable: string) {
  const { stdout } = await run(executable, ["--version"], {
    timeout: 10000,
    maxBuffer: 4096,
  });
  const cli = stdout.trim().match(/^([\d.]+)/)?.[1];
  if (!cli) throw new Error("The Claude CLI version is unavailable");
  const sdkPackage: unknown = JSON.parse(
    await fs.readFile(
      path.join(
        path.dirname(require.resolve("@anthropic-ai/claude-agent-sdk")),
        "package.json",
      ),
      "utf8",
    ),
  );
  if (!isRecord(sdkPackage) || typeof sdkPackage.version !== "string")
    throw new Error("The Claude SDK version is unavailable");
  return { cli, sdk: sdkPackage.version };
}
export async function nativeFile(
  sessionId: string,
  configDir = process.env.CLAUDE_CONFIG_DIR ||
    path.join(os.homedir(), ".claude"),
  cwd?: string,
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
      if (!isRecord(error) || error.code !== "ENOENT") throw error;
    }
  }
  const current = cwd && nativeDestination(sessionId, cwd, configDir);
  if (current && found.includes(current)) return current;
  if (found.length !== 1)
    throw new Error(
      "The complete Claude native history is missing or ambiguous",
    );
  const [only] = found;
  if (only === undefined)
    throw new Error(
      "The complete Claude native history is missing or ambiguous",
    );
  return only;
}
export function nativeDestination(
  sessionId: string,
  cwd: string,
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
  source: string,
  destination: string,
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
    if (!isRecord(error) || error.code !== "ENOENT") throw error;
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
