import { describe, expect, it } from "vitest";
import { usageAccountConnectionKey } from "./usageAccountRefresh";

describe("usage account cache refresh key", () => {
  it("changes when a participating disconnected account reconnects", () => {
    const disconnected = [{ id: "acct", disconnected: true }];
    const connected = [{ id: "acct", disconnected: false }];
    expect(usageAccountConnectionKey("acct", disconnected)).not.toBe(
      usageAccountConnectionKey("acct", connected),
    );
  });
});
