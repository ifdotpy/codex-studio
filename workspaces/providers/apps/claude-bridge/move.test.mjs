import { test } from "vitest";
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
} from "./move.mjs";

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
    const destination = nativeDestination(id, "/target", root);
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
    const first = nativeDestination(id, "/first", root);
    const second = nativeDestination(id, "/second", root);
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
    assert.equal(versions.sdk, "0.3.285");
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
  const row = (attachment) =>
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
      for (const name of [
        "mcp__studio__old",
        "mcp__studio__new",
        "mcp__studio__same",
      ])
        assert.ok(error.message.includes(name));
      assert.ok(
        error.message.includes("changed definitions [mcp__studio__same]"),
      );
      assert.ok(!error.message.includes("source-secret"));
      assert.ok(!error.message.includes("target-secret"));
      assert.deepEqual(error.data.moveRefusal, {
        kind: "studio_tools",
        sourceNames: source.map((entry) => entry.name),
        targetNames: target.map((entry) => entry.name),
        changedNames: ["mcp__studio__same"],
      });
      assert.ok(!JSON.stringify(error.data).includes("source-secret"));
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
      assert.ok(!error.message.includes("name-secret"));
      assert.deepEqual(error.data.moveRefusal.sourceNames, ["<unavailable>"]);
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
  assert.equal(catalog[0].input_schema.properties.cwd.type, "string");
  assert.throws(
    () => verifyToolProof({ tools: catalog }, [...catalog].reverse()),
    /order/,
  );
  const changed = structuredClone(catalog);
  changed[0].input_schema.properties.cwd.type = "number";
  assert.throws(() => verifyToolProof({ tools: catalog }, changed), /schemas/);
});

// Structure from the real e2187ecb native attachment. Prompt text is redacted.
test("native prompt snapshots use schema and keep deferred records separate", async () => {
  const root = await fs.mkdtemp(
    path.join(os.tmpdir(), "claude-native-snapshot-"),
  );
  const file = path.join(root, "native.jsonl");
  const session = { id: randomUUID(), model: "default", dynamicTools: [] };
  const row = (attachment) =>
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
    assert.deepEqual(
      proof.tools,
      snapshot.tools.map(({ schema, ...entry }) => ({
        ...entry,
        input_schema: schema,
      })),
    );
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
