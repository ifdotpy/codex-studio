import assert from "node:assert/strict";
import type { Message, Snapshot } from "../types";
import type { Attachment } from "./ComposerAttachments";

export function required<T>(value: T | undefined | null): T {
  assert.ok(value != null, "Expected fixture result to exist");
  return value;
}

export function componentSnapshot(
  threads: Snapshot["threads"] = [],
  runtime: Partial<Snapshot["runtime"]> = {},
): Snapshot {
  return {
    token: "test-token",
    stateDir: "test-state",
    threads,
    chats: [],
    nodes: threads,
    edges: [],
    runtime: {
      studioErrors: [],
      agents: threads,
      rooms: [],
      tasks: [],
      monitors: [],
      complaints: [],
      requests: [],
      rules: [],
      projects: [],
      peerTeams: [],
      events: [],
      work: [],
      ...runtime,
    },
  };
}

export function componentMessage(overrides: Partial<Message> = {}): Message {
  return { id: "message", role: "user", text: "", ...overrides };
}

export function componentAttachment(
  overrides: Partial<Attachment> = {},
): Attachment {
  return {
    id: "asset",
    name: "Attachment",
    mime: "image/png",
    image: false,
    size: 1,
    ...overrides,
  };
}

/** State, memo, ref and effect slots retained by the component hook fixtures. */
export type ComponentHookCell = {
  value?: unknown;
  current?: unknown;
  deps?: unknown[] | undefined;
  cleanup?: (() => void) | undefined;
};
