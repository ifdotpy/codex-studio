import { expect, it } from "vitest";
import {
  isAcceptedMessageReceiptStatus,
  selectedChatRowSelector,
} from "../scripts/verify-rust-chat-lifecycle.ts";

it("accepts durable pending inputs without treating uncertainty as delivery", () => {
  for (const status of ["pending", "queued", "accepted", "delivered", "sent"])
    expect(isAcceptedMessageReceiptStatus(status), status).toBe(true);
  for (const status of [
    undefined,
    null,
    "failed",
    "cancelled",
    "uncertain",
    "reserved",
    "dispatching",
  ])
    expect(isAcceptedMessageReceiptStatus(status), String(status)).toBe(false);
});

it("matches selected chat row values used by project and sidebar rows", () => {
  expect(selectedChatRowSelector("new-chat")).toBe(
    '[data-chat="new-chat"][aria-current="true"], [data-chat="new-chat"][aria-current="page"]',
  );
});
