import { snapshotFixture } from "../snapshotFixture";
import type { Agent, Snapshot, Request, Complaint } from "../types";
import assert from "node:assert/strict";
import {
  SIGN_IN_ALERT_GRACE_SECONDS,
  SIGN_IN_REMINDER_SECONDS,
  alertsSince,
  desktopAlerts,
  nextAlertChange,
} from "./desktopAlerts.ts";

import { it } from "vitest";

const lead = {
  id: "lead",
  name: "Main",
  isLead: true,
  status: "completed",
  lastCompletedTurn: "turn-1",
  lastCompletedTurnStatus: "completed",
  tail: "The checks pass.",
} satisfies Agent;

const snapshot = (
  threads: Agent[] = [lead],
  requests: Request[] = [],
  complaints: Complaint[] = [],
): Snapshot => snapshotFixture(threads, { requests, complaints });

it("builds completed reply notifications with a bounded title and body", () => {
  const alerts = desktopAlerts(
    snapshot([
      lead,
      { ...lead, id: "worker", isLead: false },
      { ...lead, id: "running", status: "running" },
      { ...lead, id: "old-turn", lastCompletedTurnStatus: "failed" },
      { ...lead, id: "no-turn", lastCompletedTurn: "" },
      { ...lead, id: "in-flight", inFlight: true },
      { ...lead, id: "deleted", deletedAt: 1 },
      { ...lead, id: "moved", movedTo: { server: "target", move: "move" } },
      { ...lead, id: "imported", moveImportPending: true },
      {
        ...lead,
        id: "fallback",
        name: "F".repeat(170),
        tail: "",
      },
      { ...lead, id: "long-body", tail: "b".repeat(2100) },
    ]),
  );

  assert.deepEqual(
    alerts.map(({ id }) => id),
    [
      "completed:lead:turn-1",
      "completed:fallback:turn-1",
      "completed:long-body:turn-1",
    ],
  );
  assert.deepEqual(alerts[0], {
    id: "completed:lead:turn-1",
    title: "Main: Reply ready",
    body: "The checks pass.",
    target: { agentId: "lead", section: "messages" },
  });

  assert.ok(alerts[1]);
  assert.equal(alerts[1].title.length, 160);

  assert.ok(alerts[1] !== null && alerts[1] !== undefined);
  assert.equal(alerts[1].body, "Open the chat to read the reply.");

  assert.ok(alerts[2] !== null && alerts[2] !== undefined);
  assert.equal(alerts[2].body, "b".repeat(2000));
});

it("reports failed and interrupted work using readable error fallbacks", () => {
  const alerts = desktopAlerts(
    snapshot([
      {
        ...lead,
        status: "failed",
        lastCompletedTurn: "",
        turnId: "turn-2",
        error: "Network failed",
      },
      {
        ...lead,
        id: "interrupted",
        status: "interrupted",
        lastCompletedTurn: "",
        turnId: "turn-3",
        error: { message: "Stopped by user" },
      },
      {
        ...lead,
        id: "fallback",
        status: "failed",
        lastCompletedTurn: "",
        turnId: "",
        // @ts-expect-error message is deliberately numeric in a malformed native error.
        error: { message: 7 },
      },
      {
        ...lead,
        id: "null-error",
        status: "failed",
        lastCompletedTurn: "",
        error: null,
      },
      {
        ...lead,
        id: "false-error",
        status: "failed",
        lastCompletedTurn: "",
        // @ts-expect-error error is deliberately boolean instead of a diagnostic.
        error: false,
      },
      {
        ...lead,
        id: "numeric-error",
        status: "failed",
        lastCompletedTurn: "",
        // @ts-expect-error error is deliberately numeric instead of a diagnostic.
        error: 7,
      },
      {
        ...lead,
        id: "empty-error",
        status: "failed",
        lastCompletedTurn: "",
        error: "",
      },
      {
        ...lead,
        id: "not-object-message",
        status: "failed",
        lastCompletedTurn: "",
        // @ts-expect-error error is deliberately an array instead of a diagnostic.
        error: [],
      },
    ]),
  );

  assert.deepEqual(alerts, [
    {
      id: "failed:lead:turn-2",
      title: "Main: Work stopped",
      body: "Network failed",
      target: { agentId: "lead", section: "messages" },
    },
    {
      id: "failed:interrupted:turn-3",
      title: "Main: Work stopped",
      body: "Stopped by user",
      target: { agentId: "interrupted", section: "messages" },
    },
    ...[
      "fallback",
      "null-error",
      "false-error",
      "numeric-error",
      "empty-error",
      "not-object-message",
    ].map((id) => ({
      id: `failed:${id}:${id === "fallback" ? "failed" : "failed"}`,
      title: "Main: Work stopped",
      body: id === "empty-error" ? "" : "Open the chat to check the error.",
      target: { agentId: id, section: "messages" },
    })),
  ]);
});

it("turns active requests into question or approval notifications", () => {
  const requests: Request[] = [
    {
      id: "question",
      agent: "lead",
      status: "pending",
      params: { questions: [{ id: "q", question: "Which scope?" }] },
    },
    { id: "approval", agent: "lead", status: "pending" },
    {
      id: "empty-question",
      agent: "lead",
      status: "pending",
      params: { questions: [{ id: "q", question: "" }] },
    },
    {
      id: "no-questions",
      agent: "lead",
      status: "pending",
      params: { questions: [] },
    },
    {
      id: "null-question",
      agent: "lead",
      status: "pending",
      // @ts-expect-error questions deliberately contains a null question.
      params: { questions: [null] },
    },
    {
      id: "null-questions",
      agent: "lead",
      status: "pending",
      params: { questions: null },
    },
    { id: "null-params", agent: "lead", status: "pending", params: null },
    { id: "deferred", agent: "lead", status: "pending", deferred: true },
    { id: "answered", agent: "lead", status: "answered" },
    { id: "unknown-agent", agent: "missing", status: "pending" },
    { id: "deleted-agent", agent: "gone", status: "pending" },
  ];
  const alerts = desktopAlerts(
    snapshot(
      [
        { ...lead, status: "running" },
        { ...lead, id: "gone", deletedAt: 1 },
      ],
      requests,
    ),
  );

  assert.deepEqual(
    alerts.map((alert) => [alert.id, alert.title, alert.body, alert.target]),
    [
      [
        "request:question",
        "Main: Question for you",
        "Which scope?",
        { agentId: "lead", section: "messages", itemId: "question" },
      ],
      [
        "request:approval",
        "Main: Your approval is needed",
        "Open the request to review it.",
        { agentId: "lead", section: "messages", itemId: "approval" },
      ],
      ...[
        "empty-question",
        "no-questions",
        "null-question",
        "null-questions",
        "null-params",
      ].map((id) => [
        `request:${id}`,
        "Main: Your approval is needed",
        "Open the request to review it.",
        { agentId: "lead", section: "messages", itemId: id },
      ]),
    ],
  );
});

it("uses the server's user-response flag for complaint alerts", () => {
  const complaints: Complaint[] = [
    {
      id: "for-user",
      leadId: "lead",
      recipient: "user",
      author: "worker",
      needsUserResponse: true,
      title: "Please check.",
      version: 2,
    },
    {
      id: "legacy",
      leadId: "lead",
      author: "lead",
      recipient: "user",
      needsUserResponse: true,
      title: "Legacy message.",
    },
    {
      id: "version-zero",
      leadId: "lead",
      recipient: "user",
      needsUserResponse: true,
      title: "Zero version.",
      version: 0,
    },
    {
      id: "empty-title",
      leadId: "lead",
      recipient: "user",
      needsUserResponse: true,
      title: "",
      version: 3,
    },
    {
      id: "other-author",
      leadId: "lead",
      author: "worker",
      needsUserResponse: false,
      title: "Internal.",
    },
    {
      id: "lead-recipient",
      leadId: "lead",
      author: "lead",
      recipient: "lead",
      needsUserResponse: false,
      title: "Internal.",
    },
    {
      id: "other-recipient",
      leadId: "lead",
      author: "lead",
      // @ts-expect-error recipient is deliberately outside the generated recipient set.
      recipient: "team",
      needsUserResponse: false,
      title: "Internal.",
    },
    {
      id: "no-response",
      leadId: "lead",
      recipient: "user",
      needsUserResponse: false,
      title: "Already handled.",
    },
    {
      id: "missing-lead",
      leadId: "missing",
      recipient: "user",
      needsUserResponse: true,
      title: "No target.",
    },
  ];
  const alerts = desktopAlerts(
    snapshot([{ ...lead, status: "running" }], [], complaints),
  );

  assert.deepEqual(alerts, [
    {
      id: "complaint:for-user:2",
      title: "Main: Message for you",
      body: "Please check.",
      target: { agentId: "lead", section: "messages", itemId: "for-user" },
    },
    {
      id: "complaint:legacy:0",
      title: "Main: Message for you",
      body: "Legacy message.",
      target: { agentId: "lead", section: "messages", itemId: "legacy" },
    },
    {
      id: "complaint:version-zero:0",
      title: "Main: Message for you",
      body: "Zero version.",
      target: { agentId: "lead", section: "messages", itemId: "version-zero" },
    },
    {
      id: "complaint:empty-title:3",
      title: "Main: Message for you",
      body: "Open the message to read it.",
      target: { agentId: "lead", section: "messages", itemId: "empty-title" },
    },
  ]);
});

it("keeps completed-thread alerts with empty entity collections", () => {
  const data = snapshot();
  assert.deepEqual(desktopAlerts(data), [
    {
      id: "completed:lead:turn-1",
      title: "Main: Reply ready",
      body: "The checks pass.",
      target: { agentId: "lead", section: "messages" },
    },
  ]);
});

it("does not show a complaint from before the window started when its page arrives late", () => {
  const started = 1_000_000 * 1000;
  const complaint = (version: number, updated: number): Complaint => ({
    id: "c-1",
    leadId: "lead",
    title: "Need a decision",
    needsUserResponse: true,
    created: 900_000,
    updated,
    version,
  });
  const fresh = alertsSince(started);
  const [old] = desktopAlerts(
    snapshot([lead], [], [complaint(1, 900_000)]),
  ).filter((alert) => alert.id.startsWith("complaint:"));
  assert.ok(old);
  assert.equal(old.at, 900_000);
  assert.equal(fresh(old), false);
  // A new version of the same complaint after the start is a new event.
  const [next] = desktopAlerts(
    snapshot([lead], [], [complaint(2, 1_000_010)]),
  ).filter((alert) => alert.id.startsWith("complaint:"));
  assert.ok(next);
  assert.equal(fresh(next), true);
  // A record without a time keeps the earlier rule.
  const { at: _at, ...withoutTime } = old;
  assert.equal(fresh(withoutTime), true);
});

const signIn = (fields: Partial<Request> = {}): Request => ({
  id: "card-1",
  method: "browser/open",
  agent: "lead",
  status: "pending",
  createdAt: 1000,
  params: {
    bridgeId: "b1",
    host: "id.example.com",
    expiresAt: 1600,
    source: "host",
  },
  ...fields,
});

it("alerts once with sound for a sign-in card that is still open after the grace", () => {
  assert.equal(SIGN_IN_ALERT_GRACE_SECONDS, 10);
  const ids = (now: number, request = signIn()) =>
    desktopAlerts(snapshot([lead], [request]), now)
      .map(({ id }) => id)
      .filter((id) => id !== "completed:lead:turn-1");
  // The browser of the Mac can answer the card within the grace: no alert yet.
  assert.deepEqual(ids(1009.9), []);
  assert.deepEqual(ids(1010), ["signin:card-1"]);
  const [alert] = desktopAlerts(snapshot([lead], [signIn()]), 1010).filter(
    ({ id }) => id === "signin:card-1",
  );
  assert.deepEqual(alert, {
    id: "signin:card-1",
    title: "Main: Sign-in needed",
    body: "Sign in at id.example.com. Open the card in this chat.",
    target: { agentId: "lead", section: "messages", itemId: "card-1" },
    at: 1010,
    sound: true,
  });
  // A card that someone answered, or that closed, gives no alert.
  assert.deepEqual(ids(1010, signIn({ status: "answered" })), []);
  assert.deepEqual(ids(1010, signIn({ status: "expired" })), []);
});

it("reminds once before a sign-in card closes, and not while a phone sign-in waits", () => {
  assert.equal(SIGN_IN_REMINDER_SECONDS, 300);
  const ids = (now: number, request = signIn()) =>
    desktopAlerts(snapshot([lead], [request]), now)
      .map(({ id }) => id)
      .filter((id) => id.startsWith("signin"));
  assert.deepEqual(ids(1299), ["signin:card-1"]);
  assert.deepEqual(ids(1300), ["signin:card-1", "signin-reminder:card-1"]);
  const reminder = desktopAlerts(snapshot([lead], [signIn()]), 1300).find(
    ({ id }) => id === "signin-reminder:card-1",
  );
  assert.deepEqual(reminder, {
    id: "signin-reminder:card-1",
    title: "Main: Sign-in card closes soon",
    body: "Sign in at id.example.com within 5 minutes, or the card closes.",
    target: { agentId: "lead", section: "messages", itemId: "card-1" },
    at: 1300,
    sound: true,
  });
  // The person already opened the page on a phone: the card waits for the pasted result.
  const waiting = signIn({
    params: {
      ...signIn().params,
      phase: "waiting",
      waiting: { expiresAt: 1600 },
    },
  });
  assert.deepEqual(ids(1300, waiting), ["signin:card-1"]);
  // A card whose deadline passed gives no reminder.
  assert.deepEqual(ids(1601), ["signin:card-1"]);
  // A card shorter than the grace plus the reminder lead gets only the first alert.
  const short = signIn({ params: { ...signIn().params, expiresAt: 1200 } });
  assert.deepEqual(ids(1100, short), ["signin:card-1"]);
});

it("names the next moment at which a sign-in alert becomes due", () => {
  const data = snapshot([lead], [signIn()]);
  assert.equal(nextAlertChange(data, 1005), 1010);
  assert.equal(nextAlertChange(data, 1010), 1300);
  assert.equal(nextAlertChange(data, 1300), null);
  assert.equal(nextAlertChange(snapshot([lead], []), 1005), null);
  assert.equal(
    nextAlertChange(snapshot([lead], [signIn({ status: "answered" })]), 1005),
    null,
  );
});
