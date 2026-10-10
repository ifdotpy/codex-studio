import { expect, it } from "vitest";
import {
  compactVisibleChats,
  keepCompactChat,
  shouldShowOldChatsControl,
} from "./ProjectChatRows";

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

it("applies the old-chat threshold to all chats in a project", () => {
  const now = Date.now() / 1000;
  const recent = { id: "recent", name: "Recent", updated: now };
  const recentTwo = { id: "recent-two", name: "Recent two", updated: now };
  const old = { id: "old", name: "Old", updated: 1 };
  const twoChats = [recent, old];
  expect(shouldShowOldChatsControl(twoChats.length, 3)).toBe(false);
  expect(compactVisibleChats(twoChats, true, 3)).toEqual(twoChats);

  const threeChats = [recent, recentTwo, old];
  expect(shouldShowOldChatsControl(threeChats.length, 3)).toBe(true);
  const visibleAtThree = compactVisibleChats(threeChats, true, 3);
  expect(visibleAtThree).toEqual([recent, recentTwo]);
  expect(threeChats.length - visibleAtThree.length).toBe(1);
  expect(compactVisibleChats(threeChats, true, 4)).toEqual(threeChats);
  expect(compactVisibleChats(threeChats, false, 3)).toEqual(threeChats);
});
