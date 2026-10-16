import { expect, it } from "vitest";
import { isExpectedCreateNavigationAbort } from "../scripts/verify-rust-chat-lifecycle.mjs";

const journal = {
  origin: "https://studio.example.ts.net",
  chatId: "new-chat",
  messageId: "prompt-1",
  stages: {
    create: {
      state: "confirmed",
      sentAt: "2026-10-16T10:44:17.559Z",
      confirmedAt: "2026-10-16T10:44:17.697Z",
      request: { id: "new-chat", previous: "old-chat" },
      response: { id: "new-chat" },
      navigation: {
        signal: "selected-chat-row",
        previousAgentId: "old-chat",
        chatId: "new-chat",
        observedAt: "2026-10-16T10:44:18.010Z",
      },
    },
    prompt: {
      state: "accepted",
      sentAt: "2026-10-16T10:44:18.136Z",
      request: {
        room: "new-chat",
        text: "Reply with exactly STUDIO_RUST_LUNA_OK. Do not use tools or edit files.",
      },
      receipt: { id: "prompt-1" },
    },
  },
};

const failure = {
  at: "2026-10-16T10:44:17.930Z",
  method: "GET",
  url: "https://studio.example.ts.net/api/panel?agent=old-chat",
  error: "net::ERR_ABORTED",
  stage: "create",
};

it("classifies only the old-panel abort within the confirmed create navigation", () => {
  expect(isExpectedCreateNavigationAbort(failure, journal)).toBe(true);

  for (const [label, changes] of [
    [
      "different endpoint",
      { url: failure.url.replace("/api/panel", "/api/costs") },
    ],
    ["HTTP response error", { status: 409 }],
    [
      "different origin",
      {
        url: failure.url.replace(
          "studio.example.ts.net",
          "other.example.ts.net",
        ),
      },
    ],
    ["active new chat", { url: failure.url.replace("old-chat", "new-chat") }],
    ["extra query parameter", { url: `${failure.url}&other=1` }],
    ["duplicate agent parameter", { url: `${failure.url}&agent=old-chat` }],
    ["URL fragment", { url: `${failure.url}#panel` }],
    ["other HTTP method", { method: "POST" }],
    ["different error", { error: "net::ERR_FAILED" }],
    ["different stage", { stage: "steady-idle" }],
  ]) {
    expect(
      isExpectedCreateNavigationAbort({ ...failure, ...changes }, journal),
      label,
    ).toBe(false);
  }
});

it("requires matching confirmed identity and time-bounded UI navigation evidence", () => {
  expect(
    isExpectedCreateNavigationAbort(failure, {
      ...journal,
      stages: {
        ...journal.stages,
        create: {
          ...journal.stages.create,
          response: { id: "another-chat" },
        },
      },
    }),
  ).toBe(false);

  expect(
    isExpectedCreateNavigationAbort(failure, {
      ...journal,
      stages: {
        ...journal.stages,
        create: { ...journal.stages.create, navigation: undefined },
      },
    }),
  ).toBe(true);

  expect(
    isExpectedCreateNavigationAbort(
      { ...failure, at: "2026-10-16T10:44:14.000Z" },
      journal,
    ),
  ).toBe(false);

  expect(
    isExpectedCreateNavigationAbort(
      { ...failure, at: "2026-10-16T10:44:17.930Z" },
      {
        ...journal,
        stages: {
          ...journal.stages,
          create: {
            ...journal.stages.create,
            navigation: {
              ...journal.stages.create.navigation,
              observedAt: "2026-10-16T10:44:21.000Z",
            },
          },
          prompt: undefined,
        },
      },
    ),
  ).toBe(false);
});
