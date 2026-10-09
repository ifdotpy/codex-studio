import { expect, it } from "vitest";
import { keepCompactChat } from "./ProjectChatRows";

it("keeps active, pinned, unread, team, and recent chats in compact projects", () => {
  const old = { id: "old", name: "Old chat", updated: 1 };
  const now = 200000;
  expect(keepCompactChat(old, now)).toBe(false);
  for (const extra of [
    { pinned: true },
    { unread: true },
    { team: true },
    { inFlight: true },
    { status: "running" },
    { status: "starting" },
    { status: "approval" },
    { updated: now - 86400 },
  ])
    expect(keepCompactChat({ ...old, ...extra }, now)).toBe(true);
  expect(keepCompactChat({ ...old, updated: now - 86401 }, now)).toBe(false);
});
