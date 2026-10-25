import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { test } from "vitest";
import { HUMAN_INPUT, STUDIO_EVENT_INPUT } from "./input-origin.generated.mjs";

const nativeId = "11111111-1111-4111-8111-111111111111";
const pendingId = "22222222-2222-4222-8222-222222222222";
const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
async function recover(...args) {
  const { recoverNativeSession } =
    await import("./native-session-recovery.mjs");
  return recoverNativeSession(...args);
}
async function fixture(run) {
  const temporary = await fs.mkdtemp(
    path.join(os.tmpdir(), "native-recovery-"),
  );
  const root = await fs.realpath(temporary);
  try {
    const profilePath = path.join(root, "profile");
    const cwd = path.join(root, "project");
    const bridgeRoot = path.join(root, "bridge");
    const nativeFile = path.join(
      profilePath,
      "projects",
      cwd.replace(/[^a-zA-Z0-9]/g, "-"),
      nativeId + ".jsonl",
    );
    await fs.mkdir(path.dirname(nativeFile), { recursive: true });
    await fs.mkdir(cwd);
    await fs.mkdir(path.join(bridgeRoot, "sessions"), { recursive: true });
    const rows = [
      {
        type: "user",
        sessionId: nativeId,
        cwd,
        uuid: "33333333-3333-4333-8333-333333333333",
        parentUuid: null,
        message: { content: "old question" },
      },
      {
        type: "assistant",
        sessionId: nativeId,
        cwd,
        uuid: "44444444-4444-4444-8444-444444444444",
        parentUuid: "33333333-3333-4333-8333-333333333333",
        message: {
          stop_reason: "end_turn",
          content: [{ type: "text", text: "old answer" }],
        },
      },
    ];
    const writeRows = async () =>
      fs.writeFile(
        nativeFile,
        rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
      );
    await writeRows();
    const request = {
      approval: {
        metadataLoss: true,
        actor: "operator",
        requestId: "recover-fixture",
        reason: "The user approved native-only adoption",
      },
      target: {
        agentId: "55555555-5555-4555-8555-555555555555",
        epoch: 0,
        accountKey: "claude-profile-fixture",
        provider: "claude",
        profilePath,
        cwd,
        bridgeRoot,
        nativeId,
      },
      metadata: { createdAt: 100, model: "default", name: "Recovered chat" },
      pending: [
        { id: pendingId, text: "new question", inputOrigin: HUMAN_INPUT },
      ],
    };
    const evidence = () => ({
      target: structuredClone(request.target),
      inFlight: false,
      activeQuery: false,
      backgroundTasks: 0,
      unresolvedInputs: 0,
      pending: request.pending.map((input) => ({
        ...input,
        status: "pending",
        turnId: null,
        notSent: {
          method: "thread/resume",
          requestId: "rejected-resume",
          outcome: "rejected",
          turnStartSubmitted: false,
        },
      })),
    });
    const base = path.join(bridgeRoot, "sessions", nativeId + ".json");
    await run({ request, evidence, nativeFile, rows, writeRows, base, root });
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

test("adopts an exact native conversation without inventing turns or receipts", async () =>
  fixture(async ({ request, evidence, nativeFile, base }) => {
    const before = await fs.readFile(nativeFile);
    let inspections = 0;
    const result = await recover(request, {
      inspect: async () => {
        inspections++;
        return evidence();
      },
    });
    assert.equal(inspections, 2);
    assert.equal(result.sourceSha256, hash(before));
    assert.equal(result.createdDescriptor, true);
    assert.deepEqual(await fs.readFile(nativeFile), before);
    const saved = JSON.parse(await fs.readFile(base, "utf8"));
    assert.equal(saved.format, "studio-claude-session-v1");
    assert.equal(saved.sequence, 0);
    assert.equal(saved.session.nativeId, nativeId);
    assert.equal(saved.session.id, nativeId);
    assert.equal(saved.session.started, true);
    assert.deepEqual(saved.session.turns, []);
    for (const key of [
      "controlRequests",
      "frozenNativeParams",
      "moveProof",
      "nativeMessageId",
    ])
      assert.equal(saved.session[key], undefined);
    const provenance = saved.session.nativeDescriptorRecovery;
    assert.equal(provenance.historicalBridgeMetadata, "unavailable");
    assert.equal(provenance.historicalReceipts, "unavailable");
    assert.equal(provenance.sourceSha256, hash(before));
    assert.equal(provenance.requestId, request.approval.requestId);
    assert(!JSON.stringify(saved).includes("old question"));
    assert(!JSON.stringify(saved).includes("new question"));
    assert.deepEqual(await fs.readdir(path.dirname(base)), [
      nativeId + ".json",
    ]);
    await assert.rejects(
      recover(request, { inspect: async () => evidence() }),
      /exists/,
    );
    assert.deepEqual(JSON.parse(await fs.readFile(base, "utf8")), saved);
  }));

for (const suffix of [".json", ".jsonl", ".meta.json", ".repair.json"]) {
  test(`refuses an existing ${suffix} without changing it`, async () =>
    fixture(async ({ request, evidence, base }) => {
      const file = base.slice(0, -5) + suffix;
      await fs.writeFile(file, "retained evidence");
      await assert.rejects(
        recover(request, { inspect: async () => evidence() }),
        /exists/,
      );
      assert.equal(await fs.readFile(file, "utf8"), "retained evidence");
    }));
}

test("adopts completed native history with subdirectory work and old interrupted tools", async () =>
  fixture(async ({ request, evidence, nativeFile, rows, writeRows, base }) => {
    rows[1].message.stop_reason = "tool_use";
    rows[1].message.content.push({
      type: "tool_use",
      id: "old-interrupted-tool",
    });
    rows.push(
      {
        type: "system",
        subtype: "compact_boundary",
        sessionId: nativeId,
        cwd: path.join(request.target.cwd, "src"),
      },
      {
        type: "attachment",
        sessionId: nativeId,
        cwd: path.join(request.target.cwd, "src", "layout"),
        attachment: { type: "prompt_snapshot", content: "€".repeat(70000) },
      },
      {
        type: "assistant",
        sessionId: nativeId,
        cwd: path.join(request.target.cwd, "src"),
        uuid: "66666666-6666-4666-8666-666666666666",
        parentUuid: "77777777-7777-4777-8777-777777777777",
        message: {
          stop_reason: "end_turn",
          content: [
            { type: "text", text: "completed after the old interruption" },
          ],
        },
      },
    );
    await writeRows();
    const before = await fs.readFile(nativeFile);
    await recover(request, { inspect: async () => evidence() });
    assert.deepEqual(await fs.readFile(nativeFile), before);
    const saved = JSON.parse(await fs.readFile(base, "utf8")).session;
    assert.deepEqual(saved.turns, []);
    assert.equal(saved.nativeDescriptorRecovery.sourceSha256, hash(before));
  }));

test("preserves a known Studio event input origin without relabeling or sending it", async () =>
  fixture(async ({ request, evidence, base }) => {
    request.pending[0].inputOrigin = STUDIO_EVENT_INPUT;
    const before = structuredClone(request);
    await recover(request, { inspect: async () => evidence() });
    assert.deepEqual(request, before);
    const saved = JSON.parse(await fs.readFile(base, "utf8")).session;
    assert.deepEqual(saved.turns, []);
    assert(!JSON.stringify(saved).includes(request.pending[0].text));
  }));

for (const scenario of [
  "approval",
  "account",
  "profile",
  "cwd",
  "epoch",
  "active",
  "tasks",
  "unknown-input",
  "sent-input",
  "synthetic-input",
]) {
  test(`refuses ${scenario} proof`, async () =>
    fixture(async ({ request, evidence, base }) => {
      const proof = evidence();
      if (scenario === "approval") request.approval.metadataLoss = false;
      if (scenario === "account") proof.target.accountKey = "another";
      if (scenario === "profile") proof.target.profilePath += "-another";
      if (scenario === "cwd") proof.target.cwd += "-another";
      if (scenario === "epoch") proof.target.epoch++;
      if (scenario === "active") proof.activeQuery = true;
      if (scenario === "tasks") proof.backgroundTasks = 1;
      if (scenario === "unknown-input") proof.unresolvedInputs = 1;
      if (scenario === "sent-input")
        proof.pending[0].notSent.turnStartSubmitted = true;
      if (scenario === "synthetic-input")
        proof.pending[0].inputOrigin = "studio_event";
      await assert.rejects(recover(request, { inspect: async () => proof }));
      await assert.rejects(fs.stat(base), { code: "ENOENT" });
    }));
}

for (const scenario of [
  "wrong-session",
  "wrong-cwd",
  "pending-id",
  "pending-text",
  "incomplete-turn",
  "torn",
  "malformed",
  "symlink",
]) {
  test(`refuses ${scenario} native history`, async () =>
    fixture(
      async ({ request, evidence, nativeFile, rows, writeRows, base }) => {
        if (scenario === "wrong-session") rows[0].sessionId = pendingId;
        if (scenario === "wrong-cwd") rows[0].cwd += "-another";
        if (scenario === "pending-id") rows[0].uuid = pendingId;
        if (scenario === "pending-text")
          rows[0].message.content = request.pending[0].text;
        if (scenario === "incomplete-turn")
          rows.push({
            ...rows[0],
            uuid: "66666666-6666-4666-8666-666666666666",
          });
        await writeRows();
        if (scenario === "torn") await fs.appendFile(nativeFile, '{"type":');
        if (scenario === "malformed")
          await fs.appendFile(nativeFile, "invalid\n");
        if (scenario === "symlink") {
          await fs.rename(nativeFile, nativeFile + ".original");
          await fs.symlink(nativeFile + ".original", nativeFile);
        }
        const before = await fs.readFile(nativeFile);
        await assert.rejects(
          recover(request, { inspect: async () => evidence() }),
        );
        assert.deepEqual(await fs.readFile(nativeFile), before);
        await assert.rejects(fs.stat(base), { code: "ENOENT" });
      },
    ));
}

test("rechecks idle scope and native bytes before publication", async () =>
  fixture(async ({ request, evidence, nativeFile, base }) => {
    let inspections = 0;
    await assert.rejects(
      recover(request, {
        inspect: async () => {
          if (++inspections === 2) await fs.appendFile(nativeFile, "{}\n");
          return evidence();
        },
      }),
      /changed/,
    );
    await assert.rejects(fs.stat(base), { code: "ENOENT" });
  }));

test("an exclusive publication race cannot overwrite a competing base", async () =>
  fixture(async ({ request, evidence, base }) => {
    const filesystem = {
      ...fs,
      link: async (from, to) => {
        await fs.writeFile(to, "another owner's base", { flag: "wx" });
        return fs.link(from, to);
      },
    };
    await assert.rejects(
      recover(request, { inspect: async () => evidence(), filesystem }),
      { code: "EEXIST" },
    );
    assert.equal(await fs.readFile(base, "utf8"), "another owner's base");
  }));

test("refuses changed ownership on the second inspection", async () =>
  fixture(async ({ request, evidence, base }) => {
    let inspections = 0;
    await assert.rejects(
      recover(request, {
        inspect: async () => {
          const proof = evidence();
          if (++inspections === 2) proof.target.epoch++;
          return proof;
        },
      }),
      /changed/,
    );
    await assert.rejects(fs.stat(base), { code: "ENOENT" });
  }));

test("refuses a journal that appears during staging", async () =>
  fixture(async ({ request, evidence, base }) => {
    let inspections = 0;
    await assert.rejects(
      recover(request, {
        inspect: async () => {
          if (++inspections === 2)
            await fs.writeFile(base.slice(0, -5) + ".jsonl", "new evidence");
          return evidence();
        },
      }),
      /exists/,
    );
    await assert.rejects(fs.stat(base), { code: "ENOENT" });
    assert.equal(
      await fs.readFile(base.slice(0, -5) + ".jsonl", "utf8"),
      "new evidence",
    );
  }));
