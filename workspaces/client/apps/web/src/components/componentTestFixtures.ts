import assert from "node:assert/strict";
import type { Agent, Message, Snapshot } from "../types";
import type { Attachment } from "./ComposerAttachments";

export function required<T>(value: T | undefined | null): T {
  assert.ok(value != null, "Expected fixture result to exist");
  return value;
}

// Legacy fixture records may omit ids and carry fields from an older runtime.
type RuntimeFixture = Omit<
  Partial<Snapshot["runtime"]>,
  "tasks" | "monitors" | "requests" | "complaints"
> & {
  [K in "tasks" | "monitors" | "requests" | "complaints"]?: readonly (Partial<
    Snapshot["runtime"][K][number]
  > &
    Record<string, unknown>)[];
} & Record<string, unknown>;

export function componentSnapshot(
  threads: Snapshot["threads"] = [],
  runtime: RuntimeFixture = {},
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
      ...(runtime as Partial<Snapshot["runtime"]>),
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

export function componentAgent(overrides: Partial<Agent>): Agent {
  return { id: "worker", ...overrides };
}
