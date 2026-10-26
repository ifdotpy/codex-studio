import type { Agent } from "../../src/types.ts";

/** Initial confirmed settings shared by the page and its browser assertions. */
export const initialConfirmedLead = {
  id: "lead",
  accountKey: "default",
  name: "Lead",
  isLead: true,
  status: "idle",
  subagentConcurrencyVersion: 2,
  concurrency: 32,
  agentModeRevision: 0,
} satisfies Agent;

/** Workspace identity for the isolated confirmed-settings fixture. */
export const confirmedWorkspaceId = "1234567890abcdef1234567890abcdef";
