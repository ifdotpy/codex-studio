import type { Session, ControlReceipt } from "./session-state.ts";
import type { NativeFileRecord } from "./file-restore.ts";
import type { Query, RewindFilesResult } from "@anthropic-ai/claude-agent-sdk";
import { test, vi } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { randomUUID } from "node:crypto";
import {
  prepareFileRestore,
  verifyFileRestore,
  MAX_BEFORE_RESTORE_BYTES,
} from "./file-restore.ts";
import { rollbackSession } from "./controls.mjs";

async function fixture(
  run: (fixture: Awaited<ReturnType<typeof createFixture>>) => Promise<void>,
) {
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(os.tmpdir(), "file-rewind-")),
  );
  try {
    await run(await createFixture(root));
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}
async function createFixture(root: string) {
  const cwd = path.join(root, "workspace"),
    config = path.join(root, "config"),
    state = path.join(root, "state");
  const native = randomUUID(),
    message = randomUUID();
  await fs.mkdir(cwd);
  await fs.mkdir(path.join(config, "projects", "fixture"), {
    recursive: true,
  });
  const backups = path.join(config, "file-history", native);
  await fs.mkdir(backups, { recursive: true });
  const entries = {
    "a.txt": { backupFileName: "0123456789abcdef@v1", version: 1 },
    "b.txt": { backupFileName: "fedcba9876543210@v1", version: 1 },
  };
  for (const [name, entry] of Object.entries(entries)) {
    await fs.writeFile(path.join(cwd, name), "user " + name);
    await fs.writeFile(path.join(backups, entry.backupFileName), "old " + name);
  }
  const history = path.join(config, "projects", "fixture", native + ".jsonl");
  const rows: NativeFileRecord[] = [
    {
      type: "file-history-snapshot",
      messageId: message,
      snapshot: { messageId: message, trackedFileBackups: entries },
    },
  ];
  await fs.writeFile(
    history,
    rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
  );
  const session: Session = {
    id: native,
    nativeId: native,
    cwd,
    started: true,
    turns: [
      {
        id: message,
        status: "completed",
        items: [],
        fileCheckpointMessageId: message,
        fileCheckpointSource: { nativeId: native, cwd },
      },
    ],
  };
  const messages = [
    { type: "user", uuid: message, message: { content: "edit" } },
  ];
  const deps = {
    getSessionMessages: async () => messages,
    persist: async (
      _session: Pick<Session, "controlRequests">,
    ): Promise<void> => {},
    prepareFileRestore: (
      s: Parameters<typeof prepareFileRestore>[0],
      m: Parameters<typeof prepareFileRestore>[1],
      id: string,
    ) => prepareFileRestore(s, m, id, state, config),
    verifyFileRestore,
    rewindFiles: async (
      _native: string,
      _message: string,
      { dryRun }: NonNullable<Parameters<Query["rewindFiles"]>[1]>,
    ): Promise<RewindFilesResult> => {
      if (!dryRun)
        for (const [name, entry] of Object.entries(entries))
          await fs.copyFile(
            path.join(backups, entry.backupFileName),
            path.join(cwd, name),
          );
      return { canRewind: true, skippedLinks: 0 };
    },
  };
  const receipt = (
    status: ControlReceipt["status"],
    filePlan: NonNullable<ControlReceipt["filePlan"]>,
    filesRestored = false,
  ): ControlReceipt => ({
    kind: "rollback",
    turnId: message,
    sourceNativeId: native,
    createdAt: "2026-09-01T00:00:00Z",
    retainedCount: 0,
    upToMessageId: null,
    status,
    filePlan,
    ...(filesRestored
      ? {
          result: {
            threadId: native,
            nativeId: native,
            removedTurns: 0,
            filesRestored,
          },
        }
      : {}),
  });
  return {
    receipt,
    cwd,
    config,
    state,
    backups,
    entries,
    history,
    rows,
    session,
    messages,
    deps,
    request: { requestId: "restore", turnId: message, restoreFiles: true },
  };
}

test("partial SDK success without skipped links retains context and durable before files", async () =>
  fixture(async (f) => {
    f.deps.rewindFiles = async (_native, _message, { dryRun }) => {
      if (!dryRun)
        await fs.copyFile(
          path.join(f.backups, f.entries["a.txt"].backupFileName),
          path.join(f.cwd, "a.txt"),
        );
      return { canRewind: true, skippedLinks: 0 };
    };
    await assert.rejects(
      rollbackSession(f.session, f.request, f.deps),
      /did not restore every tracked file/,
    );
    assert.equal(f.session.turns.length, 1);
    const receipt = f.session.controlRequests?.restore;
    assert.ok(receipt?.filePlan);
    assert.equal(receipt.status, "rewind_submitted");
    for (const entry of receipt.filePlan.files) {
      assert.ok(entry.backup);
      assert.equal(
        await fs.readFile(entry.backup, "utf8"),
        "user " + path.basename(entry.file),
      );
    }
    assert.deepEqual(
      JSON.parse(
        await fs.readFile(
          path.join(receipt.filePlan.directory, "manifest.json"),
          "utf8",
        ),
      ),
      receipt.filePlan,
    );
    await assert.rejects(
      rollbackSession(f.session, f.request, f.deps),
      /outcome is unknown/,
    );
  }));

test("missing native backup refuses before any SDK write or conversation fork", async () =>
  fixture(async (f) => {
    await fs.unlink(path.join(f.backups, f.entries["b.txt"].backupFileName));
    let writes = 0;
    f.deps.rewindFiles = async (_n, _m, { dryRun }) => {
      if (!dryRun) writes++;
      return { canRewind: true };
    };
    await assert.rejects(
      rollbackSession(f.session, f.request, f.deps),
      /cannot be verified/,
    );
    assert.equal(writes, 0);
    assert.equal(f.session.turns.length, 1);
    assert.equal(
      await fs.readFile(path.join(f.cwd, "a.txt"), "utf8"),
      "user a.txt",
    );
  }));

test("successful combined rewind verifies all file hashes and preserves original contents", async () =>
  fixture(async (f) => {
    const result = await rollbackSession(f.session, f.request, f.deps);
    assert.equal(result.filesRestored, true);
    assert.equal(f.session.turns.length, 0);
    assert.ok(f.session.controlRequests?.restore?.filePlan);
    for (const entry of f.session.controlRequests.restore.filePlan.files) {
      assert.ok(entry.backup);
      assert.equal(
        await fs.readFile(entry.file, "utf8"),
        "old " + path.basename(entry.file),
      );
      assert.equal(
        await fs.readFile(entry.backup, "utf8"),
        "user " + path.basename(entry.file),
      );
    }
  }));

test("delta for a new file restores its absence and rejects a changed pre-restore source", async () =>
  fixture(async (f) => {
    assert.ok(f.rows[0]?.snapshot);
    f.rows[0].snapshot.trackedFileBackups = {};
    f.rows.push({
      type: "file-history-delta",
      messageId: f.request.turnId,
      snapshotMessageId: f.request.turnId,
      trackingPath: "new.txt",
      backup: { backupFileName: null, version: 1 },
    });
    await fs.writeFile(
      f.history,
      f.rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
    );
    await fs.writeFile(path.join(f.cwd, "new.txt"), "new");
    const plan = await f.deps.prepareFileRestore(
      f.session,
      f.messages,
      f.request.turnId,
    );
    assert.equal(plan.files.length, 1);
    assert.ok(plan.files[0]?.backup);
    assert.equal(plan.files[0].expected, null);
    await fs.writeFile(path.join(f.cwd, "new.txt"), "changed");
    await assert.rejects(verifyFileRestore(plan, "before"), /Files changed/);
    await fs.unlink(path.join(f.cwd, "new.txt"));
    await verifyFileRestore(plan, "expected");
    assert.equal(await fs.readFile(plan.files[0].backup, "utf8"), "new");
  }));

test("a file first tracked after the target uses its first backup", async () =>
  fixture(async (f) => {
    const later = randomUUID();
    assert.ok(f.rows[0]?.snapshot);
    f.rows[0].snapshot.trackedFileBackups = {};
    f.rows.push({
      type: "file-history-snapshot",
      messageId: later,
      snapshot: {
        messageId: later,
        trackedFileBackups: { "a.txt": f.entries["a.txt"] },
      },
    });
    f.messages.push({
      type: "user",
      uuid: later,
      message: { content: "later" },
    });
    await fs.writeFile(
      f.history,
      f.rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
    );
    const plan = await f.deps.prepareFileRestore(
      f.session,
      f.messages,
      f.request.turnId,
    );
    assert.equal(plan.files.length, 1);
    await fs.copyFile(
      path.join(f.backups, f.entries["a.txt"].backupFileName),
      path.join(f.cwd, "a.txt"),
    );
    await verifyFileRestore(plan, "expected");
  }));

test("a failed final descriptor save retains a durable pre-rewind copy and resumes without another file write", async () =>
  fixture(async (f) => {
    let saved: Session | undefined;
    let writes = 0;
    const rewind = f.deps.rewindFiles;
    f.deps.rewindFiles = async (native, message, options) => {
      if (!options.dryRun) writes++;
      return rewind(native, message, options);
    };
    f.deps.persist = async (session) => {
      if (session.controlRequests?.restore?.status === "completed")
        throw new Error("final save failed");
      assert.ok("id" in session);
      saved = structuredClone(f.session);
    };
    await assert.rejects(
      rollbackSession(f.session, f.request, f.deps),
      /final save failed/,
    );
    assert.ok(saved?.controlRequests?.restore?.filePlan);
    assert.equal(saved.controlRequests.restore.status, "rewound");
    for (const entry of saved.controlRequests.restore.filePlan.files) {
      assert.ok(entry.backup);
      assert.equal(
        await fs.readFile(entry.backup, "utf8"),
        "user " + path.basename(entry.file),
      );
    }
    f.deps.persist = async () => {};
    assert.equal(
      (await rollbackSession(saved, f.request, f.deps)).filesRestored,
      true,
    );
    assert.equal(writes, 1);
  }));

test("file history outside the SDK message chain cannot produce a partial verification plan", async () =>
  fixture(async (f) => {
    f.rows.push({
      type: "file-history-delta",
      messageId: "omitted-system-record",
      snapshotMessageId: f.request.turnId,
      trackingPath: "other.txt",
      backup: { backupFileName: null, version: 1 },
    });
    await fs.writeFile(
      f.history,
      f.rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
    );
    await assert.rejects(
      f.deps.prepareFileRestore(f.session, f.messages, f.request.turnId),
      /cannot be verified/,
    );
  }));

test("native retention permits recent checkpoints and refuses expired checkpoints", async () =>
  fixture(async (f) => {
    for (let index = 0; index < 100; index++) {
      const message = randomUUID();
      f.messages.push({
        type: "user",
        uuid: message,
        message: { content: "later" },
      });
      f.rows.push({
        type: "file-history-snapshot",
        messageId: message,
        snapshot: { messageId: message, trackedFileBackups: f.entries },
      });
    }
    await fs.writeFile(
      f.history,
      f.rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
    );
    await assert.rejects(
      f.deps.prepareFileRestore(f.session, f.messages, f.request.turnId),
      /cannot be verified/,
    );
    const plan = await f.deps.prepareFileRestore(
      f.session,
      f.messages,
      f.messages.at(-1)?.uuid ?? "",
    );
    assert.equal(plan.files.length, 2);
  }));

test("failed backup preparation removes only its unreferenced copies", async () =>
  fixture(async (f) => {
    const original = fs.copyFile;
    let copies = 0;
    const copy = vi
      .spyOn(fs, "copyFile")
      .mockImplementation(async (...args) => {
        if (++copies === 2) throw new Error("backup disk full");
        return original(...args);
      });
    try {
      await assert.rejects(
        f.deps.prepareFileRestore(f.session, f.messages, f.request.turnId),
        /backup disk full/,
      );
      assert.deepEqual(
        await fs.readdir(path.join(f.state, "file-rewinds")),
        [],
      );
      assert.equal(
        await fs.readFile(path.join(f.cwd, "a.txt"), "utf8"),
        "user a.txt",
      );
    } finally {
      copy.mockRestore();
    }
  }));

test("backup admission refuses new copies without deleting retained recovery data", async () =>
  fixture(async (f) => {
    const retained = path.join(f.state, "file-rewinds", "retained");
    const existing = path.join(retained, "0");
    await fs.mkdir(retained, { recursive: true });
    await fs.writeFile(existing, "unique recovery data");
    const original = fs.lstat;
    const stat = vi
      .spyOn(fs, "lstat")
      .mockImplementation(async (file, ...args) => {
        const value = await original(file, ...args);
        if (file !== existing) return value;
        // Model a full byte budget without allocating a large test file.
        value.size =
          typeof value.size === "bigint"
            ? BigInt(MAX_BEFORE_RESTORE_BYTES)
            : MAX_BEFORE_RESTORE_BYTES;
        return value;
      });
    try {
      await assert.rejects(
        f.deps.prepareFileRestore(f.session, f.messages, f.request.turnId),
        /backup limit/,
      );
      assert.deepEqual(await fs.readdir(path.dirname(retained)), ["retained"]);
      assert.equal(await fs.readFile(existing, "utf8"), "unique recovery data");
      assert.equal(
        await fs.readFile(path.join(f.cwd, "a.txt"), "utf8"),
        "user a.txt",
      );
    } finally {
      stat.mockRestore();
    }
  }));

test("full quota releases settled copies, preserves unresolved copies and retries the same rewind", async () =>
  fixture(async (f) => {
    const { fileBackups, MAX_BEFORE_RESTORE_COPIES } =
      await import("./file-restore.ts");
    const completed = await f.deps.prepareFileRestore(
      f.session,
      f.messages,
      f.request.turnId,
    );
    const unresolved = await f.deps.prepareFileRestore(
      f.session,
      f.messages,
      f.request.turnId,
    );
    f.session.controlRequests = {
      settled: f.receipt("completed", completed, true),
      uncertain: f.receipt("rewind_submitted", unresolved),
    };
    // The unresolved receipt belongs to another saved chat and is never in the release proof.
    const owner = { controlRequests: f.session.controlRequests };
    delete f.session.controlRequests;
    for (let i = 2; i < MAX_BEFORE_RESTORE_COPIES; i++)
      await fs.mkdir(path.join(f.state, "file-rewinds", `before-retained${i}`));
    await assert.rejects(
      rollbackSession(f.session, f.request, f.deps),
      (error) => {
        assert.ok(error instanceof Error && "data" in error);
        assert.match(error.message, /backup limit/);
        assert.deepEqual(error.data, {
          fileRestoreOutcome: "not_applied",
          requestId: f.request.requestId,
          turnId: f.request.turnId,
        });
        return true;
      },
    );
    assert.equal(
      Object.hasOwn(f.session, "controlRequests") &&
        Reflect.get(f.session, "controlRequests")?.restore,
      undefined,
    );
    assert.equal(
      (await fileBackups(owner, null, f.state, f.deps.persist)).count,
      MAX_BEFORE_RESTORE_COPIES,
    );
    const usage = await fileBackups(
      owner,
      ["settled", "uncertain"],
      f.state,
      f.deps.persist,
    );
    assert.equal(usage.released, 1);
    assert.equal(usage.count, MAX_BEFORE_RESTORE_COPIES - 1);
    assert.ok(unresolved.files[0]?.backup);
    assert.equal(
      await fs.readFile(unresolved.files[0].backup, "utf8"),
      "user a.txt",
    );
    assert.equal(
      (await fileBackups(owner, ["settled"], f.state, f.deps.persist)).released,
      0,
    );
    assert.equal(
      (await rollbackSession(f.session, f.request, f.deps)).filesRestored,
      true,
    );
  }));

test("release refuses a completed receipt without durable settlement or with an unresolved alias", async () =>
  fixture(async (f) => {
    const { fileBackups } = await import("./file-restore.ts");
    const plan = await f.deps.prepareFileRestore(
      f.session,
      f.messages,
      f.request.turnId,
    );
    f.session.controlRequests = {
      done: f.receipt("completed", plan, true),
    };
    assert.equal(
      (await fileBackups(f.session, [], f.state, f.deps.persist)).released,
      0,
    );
    f.session.controlRequests.unknown = f.receipt("rewind_submitted", plan);
    assert.equal(
      (
        await fileBackups(
          f.session,
          ["done", "unknown"],
          f.state,
          f.deps.persist,
        )
      ).released,
      0,
    );
    assert.ok(plan.files[0]?.backup);
    assert.equal(await fs.readFile(plan.files[0].backup, "utf8"), "user a.txt");
  }));

test("concurrent admissions respect the count bound and refuse symbolic backup paths", async () =>
  fixture(async (f) => {
    const { MAX_BEFORE_RESTORE_COPIES, fileBackups } =
      await import("./file-restore.ts");
    const parent = path.join(f.state, "file-rewinds");
    await fs.mkdir(parent, { recursive: true });
    for (let i = 1; i < MAX_BEFORE_RESTORE_COPIES; i++)
      await fs.mkdir(path.join(parent, `before-retained${i}`));
    const results = await Promise.allSettled(
      [1, 2].map(() =>
        f.deps.prepareFileRestore(f.session, f.messages, f.request.turnId),
      ),
    );
    assert.equal(results.filter((r) => r.status === "fulfilled").length, 1);
    assert.equal((await fs.readdir(parent)).length, MAX_BEFORE_RESTORE_COPIES);
    await fs.symlink(f.cwd, path.join(parent, "before-link"));
    await assert.rejects(
      fileBackups(f.session, [], f.state, f.deps.persist),
      /cannot be verified/,
    );
    assert.equal(
      await fs.readFile(path.join(f.cwd, "a.txt"), "utf8"),
      "user a.txt",
    );
  }));
