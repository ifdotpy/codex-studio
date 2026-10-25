import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { test } from "vitest";
import { HUMAN_INPUT } from "./input-origin.generated.mjs";

async function invoke(argv, dependencies = {}) {
  const { main } = await import("./native-session-recovery-cli.mjs");
  let stdout = "",
    stderr = "";
  const code = await main(argv, {
    stdout: {
      write: (value) => {
        stdout += value;
      },
    },
    stderr: {
      write: (value) => {
        stderr += value;
      },
    },
    ...dependencies,
  });
  return { code, stdout, stderr };
}

async function fixture(run) {
  const temporary = await fs.mkdtemp(
    path.join(os.tmpdir(), "native-recovery-cli-"),
  );
  const root = await fs.realpath(temporary);
  try {
    const nativeId = "11111111-1111-4111-8111-111111111111";
    const target = {
      agentId: "55555555-5555-4555-8555-555555555555",
      epoch: 0,
      accountKey: "claude-profile-fixture",
      provider: "claude",
      profilePath: path.join(root, "profile"),
      cwd: path.join(root, "project"),
      bridgeRoot: path.join(root, "bridge"),
      nativeId,
    };
    const request = {
      approval: {
        metadataLoss: true,
        actor: "operator",
        requestId: "recover-cli",
        reason: "Approved native-only adoption",
      },
      target,
      metadata: { createdAt: 100, model: "default" },
      pending: [
        {
          id: "22222222-2222-4222-8222-222222222222",
          text: "private pending input",
          inputOrigin: HUMAN_INPUT,
        },
      ],
    };
    const proof = {
      target,
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
          requestId: "resume-rejected",
          outcome: "rejected",
          turnStartSubmitted: false,
        },
      })),
    };
    const nativeFile = path.join(
      target.profilePath,
      "projects",
      target.cwd.replace(/[^a-zA-Z0-9]/g, "-"),
      nativeId + ".jsonl",
    );
    await fs.mkdir(path.dirname(nativeFile), { recursive: true });
    await fs.mkdir(target.cwd);
    await fs.mkdir(path.join(target.bridgeRoot, "sessions"), {
      recursive: true,
    });
    const rows = [
      {
        type: "user",
        sessionId: nativeId,
        cwd: target.cwd,
        uuid: "33333333-3333-4333-8333-333333333333",
        message: { content: "private old input" },
      },
      {
        type: "assistant",
        sessionId: nativeId,
        cwd: target.cwd,
        uuid: "44444444-4444-4444-8444-444444444444",
        message: { content: "private old answer", stop_reason: "end_turn" },
      },
    ];
    await fs.writeFile(nativeFile, rows.map(JSON.stringify).join("\n") + "\n");
    const manifest = path.join(root, "request.json"),
      evidence = path.join(root, "proof.json");
    const inspector = path.join(root, "inspector.mjs"),
      calls = path.join(root, "calls");
    await fs.writeFile(manifest, JSON.stringify(request));
    await fs.writeFile(evidence, JSON.stringify(proof));
    await fs.writeFile(
      inspector,
      `import fs from 'node:fs/promises';
const [file,calls,mode]=process.argv.slice(2);
let count=0;try{count=Number(await fs.readFile(calls,'utf8'))}catch{}
await fs.writeFile(calls,String(count+1));
const proof=JSON.parse(await fs.readFile(file,'utf8'));
if(mode==='changed' && count)proof.target.epoch++;
console.log(JSON.stringify(proof));`,
    );
    const argv = [
      "--accept-metadata-loss",
      "--request",
      manifest,
      "--inspect",
      process.execPath,
      inspector,
      evidence,
      calls,
    ];
    const base = path.join(target.bridgeRoot, "sessions", nativeId + ".json");
    await run({
      root,
      request,
      manifest,
      evidence,
      calls,
      argv,
      base,
      nativeFile,
    });
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

test("CLI refuses missing consent before it reads a manifest or starts an inspector", async () => {
  let calls = 0;
  const result = await invoke(
    ["--request", "/missing/private.json", "--inspect", process.execPath],
    {
      execute: async () => {
        calls++;
        throw new Error("private credential");
      },
    },
  );
  assert.equal(result.code, 1);
  assert.equal(calls, 0);
  assert.equal(result.stdout, "");
  assert.match(result.stderr, /consent/);
  assert(!result.stderr.includes("private"));
});

test("CLI help states its limits without starting an inspector", async () => {
  const result = await invoke(["--help"], {
    execute: () => assert.fail("must not execute"),
  });
  assert.equal(result.code, 0);
  assert.match(result.stdout, /trusted.*read-only/i);
  assert.match(result.stdout, /Historical.*unavailable/);
  assert.match(result.stdout, /never resumes.*sends/);
});

test("CLI adopts a real fixture with two fresh Node inspector processes and prints only a receipt", async () =>
  fixture(async ({ argv, calls, base, nativeFile, request }) => {
    const before = await fs.readFile(nativeFile);
    const child = await promisify(execFile)(
      process.execPath,
      [
        fileURLToPath(
          new URL("./native-session-recovery-cli.mjs", import.meta.url),
        ),
        ...argv,
      ],
      { timeout: 10_000, maxBuffer: 64 * 1024 },
    );
    const result = { code: 0, ...child };
    assert.equal(result.code, 0, result.stderr);
    assert.equal(await fs.readFile(calls, "utf8"), "2");
    const receipt = JSON.parse(result.stdout);
    assert.equal(receipt.type, "native-session-recovery");
    assert.equal(
      receipt.sourceSha256,
      createHash("sha256").update(before).digest("hex"),
    );
    assert.equal(receipt.nativeId, request.target.nativeId);
    assert.equal(receipt.createdDescriptor, true);
    assert.equal(receipt.historicalReceipts, "unavailable");
    assert(!result.stdout.includes("private"));
    assert(!result.stdout.includes(request.target.profilePath));
    assert.equal(result.stderr, "");
    assert.deepEqual(await fs.readFile(nativeFile), before);
    const saved = JSON.parse(await fs.readFile(base, "utf8"));
    assert.deepEqual(saved.session.turns, []);
    const again = await invoke(argv);
    assert.equal(again.code, 1);
    assert.equal(again.stdout, "");
  }));

test("CLI refuses changed scope between fresh inspector processes before publication", async () =>
  fixture(async ({ argv, calls, base }) => {
    const result = await invoke([...argv, "changed"]);
    assert.equal(result.code, 1);
    assert.equal(result.stdout, "");
    assert.equal(await fs.readFile(calls, "utf8"), "2");
    await assert.rejects(fs.stat(base), { code: "ENOENT" });
  }));

test("CLI bounds and sanitizes inspector failure, invalid JSON and oversized output", async () =>
  fixture(async ({ argv, base }) => {
    for (const output of [
      new Error("private credential"),
      "private invalid JSON",
      " ".repeat(2 * 1024 * 1024 + 1),
    ]) {
      const result = await invoke(argv, {
        execute: async (_file, _args, options) => {
          assert.equal(options.timeout, 15_000);
          assert.equal(options.maxBuffer, 2 * 1024 * 1024);
          assert.equal(options.shell, false);
          assert.equal(options.killSignal, "SIGKILL");
          assert.equal(options.encoding, "buffer");
          if (output instanceof Error) throw output;
          return { stdout: Buffer.from(output) };
        },
      });
      assert.equal(result.code, 1);
      assert.equal(result.stdout, "");
      assert(!result.stderr.includes("private"));
      await assert.rejects(fs.stat(base), { code: "ENOENT" });
    }
  }));

test("CLI refuses relative paths, symlink manifests, malformed and oversized manifests before inspection", async () =>
  fixture(async ({ argv, manifest, root }) => {
    const link = path.join(root, "link.json");
    await fs.symlink(manifest, link);
    const cases = [
      argv.map((arg) => (arg === manifest ? "request.json" : arg)),
      argv.map((arg) => (arg === process.execPath ? "node" : arg)),
      argv.map((arg) => (arg === manifest ? link : arg)),
    ];
    for (const candidate of cases) {
      const result = await invoke(candidate, {
        execute: () => assert.fail("must not execute"),
      });
      assert.equal(result.code, 1);
      assert.equal(result.stdout, "");
    }
    for (const content of [
      "private invalid JSON",
      " ".repeat(2 * 1024 * 1024 + 1),
    ]) {
      await fs.writeFile(manifest, content);
      const result = await invoke(argv, {
        execute: () => assert.fail("must not execute"),
      });
      assert.equal(result.code, 1);
      assert.equal(result.stdout, "");
      assert(!result.stderr.includes("private"));
    }
  }));
