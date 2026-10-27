import type { core } from "zod";
type FixtureSchema = Omit<core.JSONSchema.ObjectSchema, "properties"> & {
  properties: Record<string, core.JSONSchema.JSONSchema>;
};
import { canonicalJson } from "./move.ts";
import { TASK_COMMAND_TOOL_SCHEMA } from "./task-command-tool.generated.ts";
import { test } from "vitest";

test("generated task advertisement converts through the real bridge SDK and preserves read arguments", async () => {
  const { createSdkMcpServer, tool } =
    await import("@anthropic-ai/claude-agent-sdk");
  const { z } = await import("zod");
  const schema = z.fromJSONSchema(TASK_COMMAND_TOOL_SCHEMA);
  assert.ok(
    schema instanceof z.ZodObject,
    "the bridge consumes an object shape",
  );
  createSdkMcpServer({
    name: "studio",
    version: "1",
    tools: [
      tool("orchestration_task", "Tasks", schema.shape, async () => ({
        content: [],
      })),
    ],
  });
  for (const input of [
    { action: "list", owner: "worker", state: "ready", cursor: "cursor" },
    { action: "get", task_id: "task" },
    { action: "history", task_id: "task", limit: 5 },
  ]) {
    assert.deepEqual(schema.parse(input), input);
  }
  for (const token of ["x", "🙂"]) {
    for (const [size, expected] of [
      [32000, true],
      [32001, false],
    ] as const) {
      assert.equal(
        schema.safeParse({
          action: "create",
          title: "Task",
          description: token.repeat(size),
        }).success,
        expected,
      );
      assert.equal(
        schema.safeParse({
          action: "cancel",
          task_id: "task",
          reason: token.repeat(size),
        }).success,
        expected,
      );
    }
  }
  // A flat SDK advertisement permits unused nullable fields; the server's
  // action-specific owner still refuses null in a consumed native field.
  assert.deepEqual(
    schema.parse({
      action: "cancel",
      task_id: "task",
      reason: "done",
      description: null,
    }),
    { action: "cancel", task_id: "task", reason: "done", description: null },
  );
});
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { randomUUID } from "node:crypto";
import {
  movePrompt,
  moveIdentity,
  nativeFile,
  nativeDestination,
  publishNative,
  moveVersions,
  savedPromptProof,
  verifyToolProof,
  studioToolCatalog,
  exactSession,
  exactProof,
  exactTurnTools,
} from "./move.ts";
import { externalAliases, externalRefusal } from "./move-external.ts";

// The same test writes these fixtures, so canonicalization preserves their type.
function canonicalFixture<T>(value: T): T {
  return JSON.parse(canonicalJson(value)!);
}

test("move exports only its native session, preserves bytes and refuses conflicts", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-move-"));
  try {
    const id = randomUUID();
    const dir = path.join(root, "projects", "source");
    await fs.mkdir(dir, { recursive: true });
    await fs.writeFile(path.join(root, "projects", ".DS_Store"), "ignored");
    const source = path.join(dir, id + ".jsonl");
    const bytes = Buffer.from('{"old":"café","uuid":"original"}\n');
    await fs.writeFile(source, bytes);
    const destination = await nativeDestination(id, "/target", root);
    assert.equal(await nativeFile(id, root), source);
    await publishNative(source, destination);
    assert.deepEqual(await fs.readFile(destination), bytes);
    await publishNative(source, destination);
    await fs.writeFile(destination, "different\n");
    await assert.rejects(publishNative(source, destination), /different bytes/);
    assert.deepEqual(movePrompt, {
      snapshot: true,
      excludeDynamicSections: true,
    });
    await assert.rejects(nativeFile("../auth", root), /identity/);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a return move extends only an exact native prefix and selects the current folder", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-move-return-"));
  try {
    const id = randomUUID();
    const first = await nativeDestination(id, "/first", root);
    const second = await nativeDestination(id, "/second", root);
    await fs.mkdir(path.dirname(first), { recursive: true });
    await fs.mkdir(path.dirname(second), { recursive: true });
    await fs.writeFile(first, '{"old":1}\n');
    await fs.writeFile(second, '{"old":1}\n{"new":2}\n');
    assert.equal(await nativeFile(id, root, "/second"), second);
    await assert.rejects(nativeFile(id, root), /ambiguous/);
    await publishNative(second, first, true);
    assert.deepEqual(await fs.readFile(first), await fs.readFile(second));
    await fs.writeFile(second, '{"changed":1}\n');
    await assert.rejects(publishNative(second, first, true), /different bytes/);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a symlinked destination folder uses the real-path key that Claude resumes", async () => {
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(os.tmpdir(), "claude-move-link-")),
  );
  try {
    const id = randomUUID();
    const project = path.join(root, "project");
    const alias = path.join(root, "link");
    await fs.mkdir(project);
    await fs.symlink(project, alias);
    const config = path.join(root, "config");
    const real = await nativeDestination(id, project, config);
    assert.equal(await nativeDestination(id, alias, config), real);
    const other = await nativeDestination(id, "/other", config);
    for (const file of [real, other]) {
      await fs.mkdir(path.dirname(file), { recursive: true });
      await fs.writeFile(file, '{"old":1}\n');
    }
    assert.equal(await nativeFile(id, config, alias), real);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a move destination key matches the SDK normalization and length limit", async () => {
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(os.tmpdir(), "claude-move-key-")),
  );
  try {
    const id = randomUUID();
    const config = path.join(root, "config");
    const nfd = path.join(root, "cafe\u0301");
    await fs.mkdir(nfd);
    // The SDK applies NFC on macOS only. "é" in NFC is one key character.
    const name = process.platform === "darwin" ? "caf-" : "cafe-";
    assert.equal(
      path.basename(path.dirname(await nativeDestination(id, nfd, config))),
      root.replace(/[^a-zA-Z0-9]/g, "-") + "-" + name,
    );
    const long = "/" + "a".repeat(199);
    assert.equal(
      path.basename(path.dirname(await nativeDestination(id, long, config))),
      "-" + "a".repeat(199),
    );
    await assert.rejects(nativeDestination(id, long + "a", config), /too long/);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("versions use the configured CLI without a model or authentication request", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-version-"));
  try {
    const cli = path.join(root, "claude");
    await fs.writeFile(
      cli,
      '#!/bin/sh\n[ "$1" = "--version" ] || exit 1\nprintf "2.1.291 (Claude Code)\\n"\n',
      { mode: 0o700 },
    );
    const versions = await moveVersions(cli);
    assert.equal(versions.cli, "2.1.291");
    const manifest = JSON.parse(
      await fs.readFile(new URL("./package.json", import.meta.url), "utf8"),
    );
    assert.equal(
      versions.sdk,
      manifest.dependencies["@anthropic-ai/claude-agent-sdk"],
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("move identity checks the signed-in organization without a model request", async () => {
  const root = await fs.mkdtemp(
    path.join(os.tmpdir(), "claude-move-identity-"),
  );
  try {
    const cli = path.join(root, "claude");
    await fs.writeFile(
      cli,
      '#!/bin/sh\n[ "$*" = "auth status --json" ] || exit 1\nprintf \'{"loggedIn":true,"authMethod":"claude.ai","email":"test@example.com","orgId":"org-test"}\\n\'\n',
      { mode: 0o700 },
    );
    assert.deepEqual(await moveIdentity(cli), {
      email: "test@example.com",
      organizationId: "org-test",
    });
    await fs.writeFile(
      cli,
      '#!/bin/sh\nprintf \'{"loggedIn":true,"authMethod":"claude.ai","email":"test@example.com"}\\n\'\n',
      { mode: 0o700 },
    );
    await assert.rejects(moveIdentity(cli), /organization are unavailable/);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("saved snapshots require a complete prompt and ordered schemas for the native identity", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-snapshot-"));
  const file = path.join(root, "native.jsonl");
  const session = { id: randomUUID(), model: "default", dynamicTools: [] };
  const row = (attachment: unknown) =>
    JSON.stringify({ type: "attachment", sessionId: session.id, attachment }) +
    "\n";
  try {
    await fs.writeFile(
      file,
      row({ type: "prompt_snapshot", systemPrompt: ["Original"] }),
    );
    await assert.rejects(
      savedPromptProof(file, session),
      /verified saved prompt snapshot/,
    );
    const attachment = {
      type: "prompt_snapshot",
      systemPrompt: ["Original"],
      tools: [
        { name: "Bash", description: "Run", input_schema: { type: "object" } },
        {
          name: "mcp__studio__move",
          input_schema: {
            type: "object",
            properties: { cwd: { type: "string" } },
          },
        },
      ],
    };
    await fs.writeFile(file, row(attachment));
    const proof = await savedPromptProof(file, session);
    assert.deepEqual(proof.tools, attachment.tools);
    assert.equal(proof.snapshotHash.length, 64);
    assert.deepEqual(
      verifyToolProof(proof, attachment.tools.slice(1), ["Bash"]),
      ["Bash"],
    );
    assert.throws(
      () => verifyToolProof(proof, [], ["Bash"]),
      /tool definitions differ/,
    );
    assert.throws(
      () => verifyToolProof(proof, attachment.tools.slice(1), []),
      /does not offer/,
    );
    assert.throws(
      () => verifyToolProof({ tools: [{ name: "mcp__external__read" }] }, []),
      /External MCP/,
    );
    await assert.rejects(
      savedPromptProof(file, { ...session, id: randomUUID() }),
      /session identity differs/,
    );
    await fs.appendFile(
      file,
      row({ type: "prompt_snapshot", systemPrompt: ["New"] }),
    );
    await assert.rejects(
      savedPromptProof(file, session),
      /verified saved prompt snapshot/,
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("tool refusal reports names and changed schemas without private values", () => {
  const source = [
    { name: "mcp__studio__old", input_schema: { token: "source-secret" } },
    { name: "mcp__studio__same", input_schema: { token: "source-secret" } },
  ];
  const target = [
    { name: "mcp__studio__new", input_schema: { token: "target-secret" } },
    { name: "mcp__studio__same", input_schema: { token: "target-secret" } },
  ];
  assert.throws(
    () => verifyToolProof({ tools: source }, target),
    (error) => {
      const refusal = error as ReturnType<typeof externalRefusal>;
      for (const name of [
        "mcp__studio__old",
        "mcp__studio__new",
        "mcp__studio__same",
      ])
        assert.ok(refusal.message.includes(name));
      assert.ok(
        refusal.message.includes("changed definitions [mcp__studio__same]"),
      );
      assert.ok(!refusal.message.includes("source-secret"));
      assert.ok(!refusal.message.includes("target-secret"));
      assert.deepEqual(refusal.data.moveRefusal, {
        kind: "studio_tools",
        sourceNames: source.map((entry) => entry.name),
        targetNames: target.map((entry) => entry.name),
        changedNames: ["mcp__studio__same"],
      });
      assert.ok(!JSON.stringify(refusal.data).includes("source-secret"));
      return true;
    },
  );
  assert.throws(
    () =>
      verifyToolProof(
        {
          tools: [
            { name: "mcp__https://host/?token=name-secret", input_schema: {} },
          ],
        },
        [],
      ),
    (error) => {
      const refusal = error as ReturnType<typeof externalRefusal>;
      assert.ok(!refusal.message.includes("name-secret"));
      assert.deepEqual(refusal.data.moveRefusal.sourceNames, ["<unavailable>"]);
      return true;
    },
  );
});

test("Studio proof reads the compiled ordered MCP schemas without model input", async () => {
  const { createSdkMcpServer, tool } =
    await import("@anthropic-ai/claude-agent-sdk");
  const { z } = await import("zod");
  const server = createSdkMcpServer({
    name: "studio",
    version: "1",
    tools: [
      tool("one", "First", { cwd: z.string() }, async () => ({ content: [] })),
      tool("two", "Second", {}, async () => ({ content: [] })),
    ],
  });
  assert.deepEqual(
    await studioToolCatalog(
      createSdkMcpServer({ name: "studio", version: "1", tools: [] }),
      0,
    ),
    [],
  );
  await assert.rejects(
    studioToolCatalog(
      createSdkMcpServer({ name: "studio", version: "1", tools: [] }),
      1,
    ),
    /Method not found/,
  );
  const catalog = await studioToolCatalog(server);
  assert.deepEqual(
    catalog.map((entry) => entry.name),
    ["mcp__studio__one", "mcp__studio__two"],
  );
  // The fixture compiled this object schema through the SDK.
  assert.ok(catalog[0]);
  const schema = catalog[0].input_schema as FixtureSchema;
  assert.ok(schema.properties.cwd);
  assert.equal(schema.properties.cwd.type, "string");
  assert.throws(
    () => verifyToolProof({ tools: catalog }, [...catalog].reverse()),
    /order/,
  );
  const changed = structuredClone(catalog);
  assert.ok(changed[0]);
  const changedSchema = changed[0].input_schema as typeof schema;
  assert.ok(changedSchema.properties.cwd);
  changedSchema.properties.cwd.type = "number";
  assert.throws(() => verifyToolProof({ tools: catalog }, changed), /schemas/);
});

// Structure from the real e2187ecb native attachment. Prompt text is redacted.
test("native prompt snapshots use schema and keep deferred records separate", async () => {
  const root = await fs.mkdtemp(
    path.join(os.tmpdir(), "claude-native-snapshot-"),
  );
  const file = path.join(root, "native.jsonl");
  const session = { id: randomUUID(), model: "default", dynamicTools: [] };
  const row = (attachment: unknown) =>
    JSON.stringify({ type: "attachment", sessionId: session.id, attachment }) +
    "\n";
  const snapshot = {
    type: "prompt_snapshot",
    systemPrompt: Array.from({ length: 13 }, () => "[redacted]"),
    tools: [
      {
        name: "Bash",
        description: "[redacted]",
        schema: {
          type: "object",
          properties: { command: { type: "string" } },
          required: ["command"],
        },
      },
    ],
    cliPrefix: "[redacted]",
    reminderFold: false,
    systemTurns: true,
    toolChangeHeader: true,
    inlineTools: false,
    keptReminders: true,
    echoWireToolInputs: true,
    contextRendering: "announced",
  };
  const deferred = {
    name: "mcp__studio__orchestration_move",
    description: "[redacted]",
    input_schema: {
      type: "object",
      properties: { cwd: { type: "string" } },
      required: ["cwd"],
    },
    eager_input_streaming: true,
    defer_loading: true,
  };
  const bytes =
    row({ type: "prompt_snapshot", systemPrompt: ["[old redacted]"] }) +
    row({ type: "deferred_tools_record", entries: [deferred] }) +
    row(snapshot);
  try {
    await fs.writeFile(file, bytes);
    const proof = await savedPromptProof(file, session);
    assert.deepEqual(proof.tools, [
      {
        name: "Bash",
        description: "[redacted]",
        input_schema: {
          type: "object",
          properties: { command: { type: "string" } },
          required: ["command"],
        },
      },
    ]);
    assert.deepEqual(proof.deferredTools, [deferred]);
    assert.equal(await fs.readFile(file, "utf8"), bytes);
    await fs.appendFile(
      file,
      row({ type: "prompt_snapshot", systemPrompt: ["[new incomplete]"] }),
    );
    await assert.rejects(
      savedPromptProof(file, session),
      /verified saved prompt snapshot/,
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("deferred schemas stay historical while the current ordered catalog is frozen", () => {
  const current = [
    {
      name: "mcp__studio__new",
      description: "Current",
      input_schema: { type: "object" },
    },
  ];
  const proof = {
    inlineTools: false,
    tools: [{ name: "Bash", input_schema: { type: "object" } }],
    deferredTools: [
      { name: "mcp__studio__old", input_schema: { type: "object" } },
    ],
  };
  assert.deepEqual(verifyToolProof(proof, current), ["Bash"]);
  assert.deepEqual(
    verifyToolProof({ ...proof, compiledTools: current }, current),
    ["Bash"],
  );
  assert.throws(
    () => verifyToolProof({ ...proof, compiledTools: current }, []),
    /tool definitions differ/,
  );
  assert.deepEqual(
    verifyToolProof(
      {
        ...proof,
        activeDeferredNames: ["Monitor", "Bash", "mcp__studio__old"],
      },
      current,
    ),
    ["Bash", "Monitor"],
  );
  assert.throws(
    () =>
      verifyToolProof({ ...proof, activeDeferredNames: ["Monitor"] }, current, [
        "Bash",
      ]),
    /does not offer.*Monitor/,
  );
});

test("an inline snapshot whose MCP tools are all deferred gets the deferred proof", async () => {
  const root = await fs.mkdtemp(
    path.join(os.tmpdir(), "claude-deferred-only-"),
  );
  const file = path.join(root, "native.jsonl");
  const session = { id: randomUUID(), model: "default", dynamicTools: [] };
  const row = (attachment: unknown) =>
    JSON.stringify({ type: "attachment", sessionId: session.id, attachment }) +
    "\n";
  const schema = { type: "object", properties: {} };
  const studio = {
    name: "mcp__studio__orchestration_teleport",
    description: "Move",
    input_schema: schema,
    defer_loading: true,
  };
  const docs = {
    name: "mcp__claude_ai_Docs__guide",
    description: "Guide",
    input_schema: schema,
  };
  const bytes =
    row({
      type: "prompt_snapshot",
      systemPrompt: ["[redacted]"],
      tools: [{ name: "Bash", input_schema: schema }],
      inlineTools: true,
    }) +
    row({
      type: "deferred_tools_delta",
      addedNames: [
        "Monitor",
        "mcp__anarlog__get_meeting",
        "mcp__claude_ai_Docs__guide",
        "mcp__studio__orchestration_teleport",
      ],
      surfacedDefinitions: [
        { name: docs.name, listing: "a1", definition: docs },
      ],
    }) +
    row({
      type: "deferred_tools_delta",
      surfacedDefinitions: [
        { name: docs.name, listing: "a1", sameAs: "earlier" },
      ],
    }) +
    row({ type: "deferred_tools_record", entries: [studio] });
  try {
    await fs.writeFile(file, bytes);
    const proof = await savedPromptProof(file, session);
    assert.equal(proof.inlineTools, false);
    assert.equal(proof.surfacedExternal, true);
    assert.deepEqual(
      proof.deferredTools!.map((t) => t.name).sort(),
      [docs.name, studio.name].sort(),
    );
    // Only servers whose definitions the history holds need a catalog on the target.
    assert.deepEqual(externalAliases(proof), ["claude_ai_Docs"]);
    // Deferred builtins stay available; the Studio catalog is compared as a whole.
    const current = [
      { name: studio.name, description: "Move", input_schema: schema },
    ];
    assert.deepEqual(verifyToolProof(proof, current, ["Bash", "Monitor"]), [
      "Bash",
      "Monitor",
    ]);
    assert.throws(
      () => verifyToolProof({ ...proof, compiledTools: current }, []),
      /tool definitions differ/,
    );
    // A snapshot with an inline MCP tool keeps the inline proof.
    await fs.writeFile(
      file,
      row({
        type: "prompt_snapshot",
        systemPrompt: ["[redacted]"],
        inlineTools: true,
        tools: [
          { name: "Bash", input_schema: schema },
          { name: studio.name, input_schema: schema },
        ],
      }) + row({ type: "deferred_tools_record", entries: [studio] }),
    );
    const inline = await savedPromptProof(file, session);
    assert.equal(inline.inlineTools, undefined);
    assert.equal(inline.deferredTools, undefined);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a catalog that crossed the paired channel with sorted keys still matches", () => {
  const schema = {
    type: "object",
    properties: { cwd: { type: "string" }, server: { type: "string" } },
    required: ["server"],
    additionalProperties: false,
  };
  const current = [
    { name: "mcp__studio__a", description: "A", input_schema: schema },
    {
      name: "mcp__studio__b",
      description: "B",
      input_schema: { type: "object" },
    },
  ];
  const sorted = canonicalFixture(current);
  const [first, second] = sorted;
  assert.ok(first && second);
  const proof = {
    inlineTools: false,
    tools: [{ name: "Bash", input_schema: { type: "object" } }],
    compiledTools: sorted,
  };
  assert.deepEqual(verifyToolProof(proof, current), ["Bash"]);
  // A real change and a changed tool order still refuse.
  assert.throws(
    () =>
      verifyToolProof(
        {
          ...proof,
          compiledTools: [{ ...first, description: "Changed" }, second],
        },
        current,
      ),
    /tool definitions differ/,
  );
  assert.throws(
    () =>
      verifyToolProof({ ...proof, compiledTools: [second, first] }, current),
    /tool definitions differ/,
  );
});

test("a move restores the exact tool order that the paired channel sorted", () => {
  const original = [
    {
      name: "orchestration_teleport",
      description: "Move",
      inputSchema: {
        type: "object",
        properties: { server: { type: "string" }, cwd: { type: "string" } },
        required: ["server", "cwd"],
      },
    },
  ];
  // What the canonical JSON of the channel delivers.
  const moved = canonicalFixture({
    id: "s",
    dynamicTools: original,
    dynamicToolsJson: JSON.stringify(original),
  });
  assert.notEqual(JSON.stringify(moved.dynamicTools), JSON.stringify(original));
  assert.equal(
    JSON.stringify(exactSession(moved).dynamicTools),
    JSON.stringify(original),
  );
  assert.ok(original[0]);
  const compiled = [
    {
      name: "mcp__studio__orchestration_teleport",
      description: "Move",
      input_schema: original[0].inputSchema,
    },
  ];
  const proof = canonicalFixture({
    compiledTools: compiled,
    compiledToolsJson: JSON.stringify(compiled),
  });
  assert.equal(
    JSON.stringify(exactProof(proof).compiledTools),
    JSON.stringify(compiled),
  );
  // Text that does not match its sorted copy is refused, and old peers keep their copy.
  assert.throws(
    () =>
      exactSession({
        ...moved,
        dynamicToolsJson: JSON.stringify([
          { ...original[0], description: "Other" },
        ]),
      }),
    /exact Studio tools differ/,
  );
  assert.equal(
    exactSession({ dynamicTools: moved.dynamicTools }).dynamicTools,
    moved.dynamicTools,
  );
});

test("builtin tools stay the same while connectors drop and add their names around turns", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-flicker-"));
  const file = path.join(root, "native.jsonl");
  const session = { id: randomUUID(), model: "default", dynamicTools: [] };
  const row = (attachment: unknown) =>
    JSON.stringify({ type: "attachment", sessionId: session.id, attachment }) +
    "\n";
  const schema = { type: "object" };
  const studio = {
    name: "mcp__studio__orchestration_teleport",
    input_schema: schema,
  };
  const start =
    row({
      type: "prompt_snapshot",
      systemPrompt: ["[redacted]"],
      tools: [{ name: "Bash", input_schema: schema }],
      inlineTools: true,
    }) +
    row({
      type: "deferred_tools_delta",
      addedNames: [
        "Monitor",
        "ListMcpResourcesTool",
        "mcp__studio__orchestration_teleport",
      ],
    }) +
    row({ type: "deferred_tools_record", entries: [studio] });
  const dropped = row({
    type: "deferred_tools_delta",
    removedNames: ["ListMcpResourcesTool"],
  });
  const added = row({
    type: "deferred_tools_delta",
    addedNames: ["ListMcpResourcesTool"],
  });
  try {
    const builtins = [];
    for (const bytes of [start + dropped, start + dropped + added]) {
      await fs.writeFile(file, bytes);
      const proof = await savedPromptProof(file, session);
      builtins.push(
        verifyToolProof(proof, [{ name: studio.name, input_schema: schema }]),
      );
    }
    assert.deepEqual(builtins[0], builtins[1]);
    assert.deepEqual(builtins[0], ["Bash", "Monitor", "ListMcpResourcesTool"]);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("later turns of a moved session keep the exact tool order the server sorted", async () => {
  const { createSdkMcpServer, tool } =
    await import("@anthropic-ai/claude-agent-sdk");
  const { z } = await import("zod");
  // The bridge's Studio server compiles each definition from its JSON schema.
  const compile = (
    definitions: {
      name: string;
      description: string;
      inputSchema: Parameters<typeof z.fromJSONSchema>[0];
    }[],
  ) =>
    studioToolCatalog(
      createSdkMcpServer({
        name: "studio",
        version: "1",
        tools: definitions.map((definition) => {
          const schema = z.fromJSONSchema(definition.inputSchema);
          assert.ok(schema instanceof z.ZodObject);
          return tool(
            definition.name,
            definition.description,
            (schema as import("zod").ZodObject).shape,
            async () => ({ content: [] }),
          );
        }),
      }),
      definitions.length,
    );
  const source: Parameters<typeof compile>[0] = [
    {
      name: "orchestration_move",
      description: "Move",
      inputSchema: {
        type: "object",
        properties: { server: { type: "string" }, cwd: { type: "string" } },
        required: ["server", "cwd"],
        additionalProperties: false,
      },
    },
  ];
  const session = { dynamicToolsJson: JSON.stringify(source) };
  // The server stores records with sorted keys and sends that copy on later turns.
  const sorted = canonicalFixture(source);
  const proof = await compile(source);
  const sortedProof = await compile(sorted);
  assert.ok(sortedProof[0] && proof[0]);
  assert.notDeepEqual(
    sortedProof[0].input_schema.required,
    proof[0].input_schema.required,
  );
  assert.deepEqual(await compile(exactTurnTools(session, sorted)), proof);
  verifyToolProof(
    { tools: [], inlineTools: false, compiledTools: proof },
    await compile(exactTurnTools(session, sorted)),
  );
  // Changed content stays as the server sent it, so the proof check still refuses it.
  const changed = [{ ...sorted[0], description: "Changed" }];
  assert.equal(exactTurnTools(session, changed), changed);
  assert.equal(exactTurnTools({}, sorted), sorted);
});

test.each(
  [[], 42, null, false, "invalid"].map((invalidRow) => ({ invalidRow })),
)(
  "non-object native row $invalidRow cannot certify a saved prompt",
  async ({ invalidRow }) => {
    const root = await fs.mkdtemp(
      path.join(os.tmpdir(), "claude-prompt-boundary-"),
    );
    const file = path.join(root, "native.jsonl");
    const session = { id: randomUUID(), model: "default", dynamicTools: [] };
    const row = {
      type: "attachment",
      sessionId: session.id,
      attachment: {
        type: "prompt_snapshot",
        systemPrompt: ["Complete prompt"],
        tools: [{ name: "Bash", input_schema: { type: "object" } }],
      },
    };
    try {
      await fs.writeFile(file, JSON.stringify(row) + "\n");
      assert.equal(
        (await savedPromptProof(file, session)).snapshotHash.length,
        64,
      );
      await fs.appendFile(file, JSON.stringify(invalidRow) + "\n");
      await assert.rejects(
        savedPromptProof(file, session),
        /complete native snapshot history is unavailable/,
      );
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  },
);
