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
} from "./file-restore.mjs";
import { rollbackSession } from "./controls.mjs";

async function fixture(run) {
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(os.tmpdir(), "file-rewind-")),
  );
  try {
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
      await fs.writeFile(
        path.join(backups, entry.backupFileName),
        "old " + name,
      );
    }
    const history = path.join(config, "projects", "fixture", native + ".jsonl");
    const rows = [
      {
        type: "file-history-snapshot",
        messageId: message,
        snapshot: { messageId: message, trackedFileBackups: entries },
      },
    ];
    await fs.writeFile(history, rows.map(JSON.stringify).join("\n") + "\n");
    const session = {
      id: native,
      nativeId: native,
      cwd,
      started: true,
      turns: [
        {
          id: message,
          status: "completed",
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
      persist: async () => {},
      prepareFileRestore: (s, m, id) =>
        prepareFileRestore(s, m, id, state, config),
      verifyFileRestore,
      rewindFiles: async (_native, _message, { dryRun }) => {
        if (!dryRun)
          for (const [name, entry] of Object.entries(entries))
            await fs.copyFile(
              path.join(backups, entry.backupFileName),
              path.join(cwd, name),
            );
        return { canRewind: true, skippedLinks: 0 };
      },
    };
    await run({
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
    });
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
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
    const receipt = f.session.controlRequests.restore;
    assert.equal(receipt.status, "rewind_submitted");
    for (const entry of receipt.filePlan.files)
      assert.equal(
        await fs.readFile(entry.backup, "utf8"),
        "user " + path.basename(entry.file),
      );
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
    for (const entry of f.session.controlRequests.restore.filePlan.files) {
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
    f.rows[0].snapshot.trackedFileBackups = {};
    f.rows.push({
      type: "file-history-delta",
      messageId: f.request.turnId,
      snapshotMessageId: f.request.turnId,
      trackingPath: "new.txt",
      backup: { backupFileName: null, version: 1 },
    });
    await fs.writeFile(f.history, f.rows.map(JSON.stringify).join("\n") + "\n");
    await fs.writeFile(path.join(f.cwd, "new.txt"), "new");
    const plan = await f.deps.prepareFileRestore(
      f.session,
      f.messages,
      f.request.turnId,
    );
    assert.equal(plan.files.length, 1);
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
    await fs.writeFile(f.history, f.rows.map(JSON.stringify).join("\n") + "\n");
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
    let saved,
      writes = 0;
    const rewind = f.deps.rewindFiles;
    f.deps.rewindFiles = async (native, message, options) => {
      if (!options.dryRun) writes++;
      return rewind(native, message, options);
    };
    f.deps.persist = async (session) => {
      if (session.controlRequests.restore?.status === "completed")
        throw new Error("final save failed");
      saved = structuredClone(session);
    };
    await assert.rejects(
      rollbackSession(f.session, f.request, f.deps),
      /final save failed/,
    );
    assert.equal(saved.controlRequests.restore.status, "rewound");
    for (const entry of saved.controlRequests.restore.filePlan.files)
      assert.equal(
        await fs.readFile(entry.backup, "utf8"),
        "user " + path.basename(entry.file),
      );
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
    await fs.writeFile(f.history, f.rows.map(JSON.stringify).join("\n") + "\n");
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
    await fs.writeFile(f.history, f.rows.map(JSON.stringify).join("\n") + "\n");
    await assert.rejects(
      f.deps.prepareFileRestore(f.session, f.messages, f.request.turnId),
      /cannot be verified/,
    );
    const plan = await f.deps.prepareFileRestore(
      f.session,
      f.messages,
      f.messages.at(-1).uuid,
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
        return {
          ...value,
          size: MAX_BEFORE_RESTORE_BYTES,
          isFile: () => true,
          isSymbolicLink: () => false,
        };
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
