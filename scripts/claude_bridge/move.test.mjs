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
