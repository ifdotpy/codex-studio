import fs from "node:fs/promises";
import { constants } from "node:fs";
import path from "node:path";
import { createHash } from "node:crypto";
import { TextDecoder } from "node:util";
import { nativeUuid } from "./native-history.mjs";
import { nativeDestination } from "./move.mjs";
import { createSessionStore } from "./session-store.mjs";
import { HUMAN_INPUT, STUDIO_EVENT_INPUT } from "./input-origin.generated.mjs";

// Full-file recovery admission bound, in bytes. Unlike restart classification,
// this operation must inspect the entire history, never only its tail.
export const MAX_NATIVE_RECOVERY_BYTES = 256 * 1024 * 1024;
// Working buffer of the native history reader, in bytes.
const READ_CHUNK_BYTES = 64 * 1024;
const TARGET_FIELDS = [
  "agentId",
  "epoch",
  "accountKey",
  "provider",
  "profilePath",
  "cwd",
  "bridgeRoot",
  "nativeId",
];
const SIDECAR_SUFFIXES = [".json", ".jsonl", ".meta.json", ".repair.json"];
const requireProof = (condition, message) => {
  if (!condition) throw new Error(message);
};
const text = (value, limit = 256) =>
  typeof value === "string" && value.length > 0 && value.length <= limit;

function validateRequest(request) {
  const { approval, target, metadata, pending } = request;
  requireProof(
    approval?.metadataLoss === true &&
      text(approval.actor) &&
      text(approval.reason, 2048) &&
      text(approval.requestId),
    "Explicit metadata-loss approval is required",
  );
  requireProof(
    target &&
      nativeUuid(target.agentId) &&
      nativeUuid(target.nativeId) &&
      text(target.accountKey) &&
      target.provider === "claude" &&
      Number.isSafeInteger(target.epoch) &&
      target.epoch >= 0,
    "Invalid recovery target identity",
  );
  requireProof(
    TARGET_FIELDS.length === Object.keys(target).length &&
      TARGET_FIELDS.every((key) => Object.hasOwn(target, key)),
    "Unexpected recovery target fields",
  );
  for (const key of ["profilePath", "cwd", "bridgeRoot"])
    requireProof(
      path.isAbsolute(target[key] || "") &&
        path.resolve(target[key]) === target[key],
      "An absolute canonical target path is required",
    );
  requireProof(
    metadata &&
      Number.isFinite(metadata.createdAt) &&
      metadata.createdAt >= 0 &&
      text(metadata.model) &&
      (metadata.name === undefined || text(metadata.name)) &&
      Object.keys(metadata).every((key) =>
        ["createdAt", "model", "name"].includes(key),
      ),
    "Invalid descriptor metadata",
  );
  requireProof(
    Array.isArray(pending) &&
      pending.length > 0 &&
      pending.length <= 100 &&
      new Set(pending.map((input) => input.id)).size === pending.length,
    "Exact pending input identities are required",
  );
  for (const input of pending)
    requireProof(
      nativeUuid(input.id) &&
        text(input.text, 1024 * 1024) &&
        [HUMAN_INPUT, STUDIO_EVENT_INPUT].includes(input.inputOrigin) &&
        Object.keys(input).every((key) =>
          ["id", "text", "inputOrigin"].includes(key),
        ),
      "Only plain UUID input with a known origin is supported",
    );
}

async function verifyInspection(request, inspect) {
  const proof = await inspect();
  requireProof(
    proof &&
      TARGET_FIELDS.every((key) => proof.target?.[key] === request.target[key]),
    "The recovery target changed",
  );
  requireProof(
    proof.inFlight === false &&
      proof.activeQuery === false &&
      proof.backgroundTasks === 0 &&
      proof.unresolvedInputs === 0,
    "The native session is not proven idle",
  );
  requireProof(
    Array.isArray(proof.pending) &&
      proof.pending.length === request.pending.length,
    "Pending input evidence is incomplete",
  );
  for (const input of request.pending) {
    const matches = proof.pending.filter((item) => item.id === input.id);
    const item = matches[0];
    requireProof(
      matches.length === 1 &&
        item.text === input.text &&
        item.status === "pending" &&
        item.turnId === null &&
        item.inputOrigin === input.inputOrigin &&
        item.notSent?.method === "thread/resume" &&
        text(item.notSent.requestId) &&
        item.notSent.outcome === "rejected" &&
        item.notSent.turnStartSubmitted === false,
      "Pending input is not proven unsent",
    );
  }
}

async function canonicalDirectory(filesystem, directory) {
  const stat = await filesystem.lstat(directory);
  requireProof(
    stat.isDirectory() &&
      !stat.isSymbolicLink() &&
      (await filesystem.realpath(directory)) === directory,
    "A target directory is missing or uses a symlink",
  );
}

async function absentSidecars(filesystem, directory, id) {
  for (const suffix of SIDECAR_SUFFIXES) {
    try {
      await filesystem.lstat(path.join(directory, id + suffix));
    } catch (error) {
      if (error.code === "ENOENT") continue;
      throw error;
    }
    throw Object.assign(new Error("Studio session evidence already exists"), {
      code: "EEXIST",
    });
  }
}

const sameFile = (before, after) =>
  before.dev === after.dev &&
  before.ino === after.ino &&
  before.size === after.size &&
  before.mtimeMs === after.mtimeMs &&
  before.ctimeMs === after.ctimeMs;

function withinWorkspace(cwd, workspace) {
  if (typeof cwd !== "string" || !path.isAbsolute(cwd)) return false;
  const relative = path.relative(workspace, path.resolve(cwd));
  return (
    relative !== ".." &&
    !relative.startsWith(".." + path.sep) &&
    !path.isAbsolute(relative)
  );
}

async function inspectNative(filesystem, file, request) {
  requireProof(
    (await filesystem.realpath(file)) === file,
    "Native history uses a symlink",
  );
  const handle = await filesystem.open(
    file,
    constants.O_RDONLY | constants.O_NOFOLLOW,
  );
  try {
    const before = await handle.stat();
    requireProof(
      before.isFile() &&
        before.size > 0 &&
        before.size <= MAX_NATIVE_RECOVERY_BYTES,
      "Native history exceeds the recovery bound or is empty",
    );
    const digest = createHash("sha256");
    const decoder = new TextDecoder("utf-8", { fatal: true });
    const buffer = Buffer.alloc(READ_CHUNK_BYTES);
    let remainder = "",
      offset = 0,
      last = null,
      users = 0;
    const record = (line) => {
      let row;
      try {
        row = JSON.parse(line);
      } catch {
        throw new Error("Invalid native history record");
      }
      requireProof(
        row && typeof row === "object" && !Array.isArray(row),
        "Invalid native history record",
      );
      for (const key of ["sessionId", "session_id"])
        requireProof(
          row[key] === undefined || row[key] === request.target.nativeId,
          "Native history belongs to another session",
        );
      requireProof(
        row.cwd === undefined || withinWorkspace(row.cwd, request.target.cwd),
        "Native history belongs to another workspace",
      );
      for (const input of request.pending) {
        requireProof(
          row.uuid !== input.id,
          "The pending input is already present in native history",
        );
        if (row.type === "user") {
          const content = row.message?.content;
          const blocks =
            typeof content === "string"
              ? [content]
              : Array.isArray(content)
                ? content
                    .filter((block) => block.type === "text")
                    .map((block) => block.text)
                : [];
          requireProof(
            !blocks.includes(input.text) && blocks.join("\n") !== input.text,
            "The pending input text is already present in native history",
          );
        }
      }
      if (!["user", "assistant"].includes(row.type) || row.isSidechain === true)
        return;
      requireProof(
        row.sessionId === request.target.nativeId &&
          withinWorkspace(row.cwd, request.target.cwd) &&
          nativeUuid(row.uuid) &&
          row.message &&
          typeof row.message === "object",
        "Native conversation scope is incomplete",
      );
      requireProof(
        last !== null || row.type === "user",
        "Native history has no initial user record",
      );
      users += Number(row.type === "user");
      // Old interrupted tools and compacted parent boundaries do not describe
      // the last turn. This descriptor recovers neither their state nor receipts.
      last = row;
    };
    while (offset < before.size) {
      const { bytesRead } = await handle.read(
        buffer,
        0,
        Math.min(buffer.length, before.size - offset),
        offset,
      );
      requireProof(bytesRead > 0, "Native history changed during recovery");
      offset += bytesRead;
      digest.update(buffer.subarray(0, bytesRead));
      remainder += decoder.decode(buffer.subarray(0, bytesRead), {
        stream: true,
      });
      let start = 0,
        end;
      while ((end = remainder.indexOf("\n", start)) >= 0) {
        record(remainder.slice(start, end));
        start = end + 1;
      }
      remainder = remainder.slice(start);
    }
    remainder += decoder.decode();
    requireProof(remainder === "", "Native history has a partial final record");
    requireProof(
      users > 0 &&
        last?.type === "assistant" &&
        last.message.stop_reason === "end_turn",
      "Native history does not prove a completed turn",
    );
    const after = await handle.stat();
    const named = await filesystem.lstat(file);
    requireProof(
      !named.isSymbolicLink() &&
        sameFile(before, after) &&
        sameFile(before, named),
      "Native history changed during recovery",
    );
    return {
      sourceSha256: digest.digest("hex"),
      sourceBytes: offset,
      identity: before,
    };
  } finally {
    await handle.close();
  }
}

/**
 * Adopt only a user-approved native conversation whose Studio descriptor is lost.
 * inspect is a trusted, read-only caller port. It must independently check current
 * registry ownership, agent epoch, native query/tasks, and rejected-resume input
 * evidence. A caller assertion is not a provider delivery receipt. This operation
 * creates no historical turns, input receipts, file checkpoints, or model query.
 * The exclusive base link publishes the descriptor. Success requires synced
 * descriptor bytes and a synced destination directory. A retry refuses an existing base;
 * an operator can inspect its recovery requestId after a lost success response.
 */
export async function recoverNativeSession(
  request,
  { inspect, filesystem = fs } = {},
) {
  // Freeze caller-owned data before the first await.
  request = structuredClone(request);
  validateRequest(request);
  requireProof(
    typeof inspect === "function",
    "A read-only ownership inspector is required",
  );
  const { target, approval, metadata } = request;
  const directory = path.join(target.bridgeRoot, "sessions");
  for (const location of [
    target.profilePath,
    target.cwd,
    target.bridgeRoot,
    directory,
  ])
    await canonicalDirectory(filesystem, location);
  await verifyInspection(request, inspect);
  await absentSidecars(filesystem, directory, target.nativeId);
  const nativeFile = nativeDestination(
    target.nativeId,
    target.cwd,
    target.profilePath,
  );
  const source = await inspectNative(filesystem, nativeFile, request);
  const staging = await filesystem.mkdtemp(
    path.join(target.bridgeRoot, ".native-session-recovery-"),
  );
  try {
    const at = new Date().toISOString();
    const descriptor = {
      id: target.nativeId,
      nativeId: target.nativeId,
      cwd: target.cwd,
      nativeHistoryCwd: target.cwd,
      started: true,
      turns: [],
      ...metadata,
      updatedAt: Date.now() / 1000,
      nativeDescriptorRecovery: {
        at,
        actor: approval.actor,
        requestId: approval.requestId,
        reason: approval.reason,
        agentId: target.agentId,
        epoch: target.epoch,
        accountKey: target.accountKey,
        sourceSha256: source.sourceSha256,
        sourceBytes: source.sourceBytes,
        historicalBridgeMetadata: "unavailable",
        historicalReceipts: "unavailable",
      },
    };
    const store = createSessionStore(staging);
    await store.persist(descriptor);
    await store.drain();
    // The session writer drains its rename, not a durable file sync.
    const stagedFile = path.join(
      staging,
      "sessions",
      target.nativeId + ".json",
    );
    const staged = await filesystem.open(
      stagedFile,
      constants.O_RDONLY | constants.O_NOFOLLOW,
    );
    try {
      await staged.sync();
    } finally {
      await staged.close();
    }
    await verifyInspection(request, inspect);
    const checked = await inspectNative(filesystem, nativeFile, request);
    requireProof(
      source.sourceSha256 === checked.sourceSha256 &&
        sameFile(source.identity, checked.identity),
      "Native history changed before publication",
    );
    for (const location of [
      target.profilePath,
      target.cwd,
      target.bridgeRoot,
      directory,
    ])
      await canonicalDirectory(filesystem, location);
    await absentSidecars(filesystem, directory, target.nativeId);
    const destination = await filesystem.open(
      directory,
      constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW,
    );
    try {
      await filesystem.link(
        stagedFile,
        path.join(directory, target.nativeId + ".json"),
      );
      // After the link, any failure preserves the base. Never remove it or
      // report success before its request identity has a durable directory entry.
      await destination.sync();
    } finally {
      await destination.close();
    }
    return {
      createdDescriptor: true,
      requestId: approval.requestId,
      nativeId: target.nativeId,
      sourceSha256: source.sourceSha256,
      sourceBytes: source.sourceBytes,
      historicalBridgeMetadata: "unavailable",
      historicalReceipts: "unavailable",
    };
  } finally {
    await filesystem.rm(staging, { recursive: true, force: true });
  }
}
