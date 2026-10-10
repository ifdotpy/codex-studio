import assert from "node:assert/strict";
import { desktopAlerts } from "./desktopAlerts.ts";

import { it } from "vitest";

const lead = {
  id: "lead",
  name: "Main",
  isLead: true,
  status: "completed",
  lastCompletedTurn: "turn-1",
  lastCompletedTurnStatus: "completed",
  tail: "The checks pass.",
};

const snapshot = (threads = [lead], requests = [], complaints = []) => ({
  token: "",
  stateDir: "/fixture",
  threads,
  chats: [],
  nodes: threads,
  edges: [],
  runtime: {
    agents: threads,
    rooms: [],
    tasks: [],
    monitors: [],
    requests,
    complaints,
    rules: [],
    projects: [],
    peerTeams: [],
    events: [],
    work: [],
  },
});

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
      { ...lead, id: "moved", movedTo: { server: "target" } },
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
  assert.equal(alerts[1].title.length, 160);
  assert.equal(alerts[1].body, "Open the chat to read the reply.");
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
        error: false,
      },
      {
        ...lead,
        id: "numeric-error",
        status: "failed",
        lastCompletedTurn: "",
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
  const requests = [
    {
      id: "question",
      agent: "lead",
      status: "pending",
      params: { questions: [{ question: "Which scope?" }] },
    },
    { id: "approval", agent: "lead", status: "pending" },
    {
      id: "empty-question",
      agent: "lead",
      status: "pending",
      params: { questions: [{ question: "" }] },
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
  const complaints = [
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
