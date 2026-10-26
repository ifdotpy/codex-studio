import { expect, it, vi } from "vitest";
import { createScreenChannel } from "./screenChannel";
it("keeps commands and publication with the original connection without replay", async () => {
  const publish = vi.fn();
  const a = createScreenChannel("a", publish),
    b = createScreenChannel("b", publish);
  const runA = vi.fn().mockRejectedValue(new Error("unknown outcome")),
    runB = vi.fn();
  const unbind = a.bind(runA);
  b.bind(runB);
  await expect(
    a.execute({ action: "remove-chat", id: "same", room: false }),
  ).rejects.toThrow("unknown outcome");
  expect(runA).toHaveBeenCalledOnce();
  expect(runB).not.toHaveBeenCalled();
  a.publish({ kind: "navigation", serverId: "b" });
  expect(publish).toHaveBeenLastCalledWith({
    kind: "navigation",
    serverId: "a",
  });
  unbind();
  await expect(a.execute({ action: "open", id: "same" })).rejects.toThrow(
    "not sent",
  );
  const replacement = vi.fn();
  a.bind(replacement);
  unbind();
  await a.execute({ action: "open", id: "same" });
  expect(replacement).toHaveBeenCalledOnce();
  a.close();
  a.publish({ kind: "late" });
  expect(publish).toHaveBeenCalledOnce();
  await expect(a.execute({ action: "open", id: "same" })).rejects.toThrow(
    "not sent",
  );
  expect(() => a.bind(runA)).toThrow("closed");
  await b.execute({ action: "open", id: "same" });
  expect(runB).toHaveBeenCalledOnce();
  b.close();
});
it("routes terminal controls only to the mounted terminal of their owner", async () => {
  const a = createScreenChannel("a", () => {}),
    b = createScreenChannel("b", () => {});
  a.bind(() => {});
  b.bind(() => {});
  const terminalA = vi.fn(),
    terminalB = vi.fn();
  const stop = a.bindTerminal(terminalA);
  b.bindTerminal(terminalB);
  await a.execute({ action: "terminal", operation: "new" });
  expect(terminalA).toHaveBeenCalledWith("new");
  expect(terminalB).not.toHaveBeenCalled();
  stop();
  await expect(
    a.execute({ action: "terminal", operation: "new" }),
  ).rejects.toThrow("not sent");
  expect(terminalA).toHaveBeenCalledOnce();
  a.close();
  b.close();
});

it("settles falsy search replies as empty results while preserving their error", async () => {
  const { screenSearchResults } = await import("./screenChannel");
  for (const results of [false, 0, "", null, undefined]) {
    expect(
      screenSearchResults({
        kind: "studio-server-search-results",
        requestId: "request",
        results,
        error: "Search failed",
      }),
    ).toEqual({ results: [], error: "Search failed" });
  }
});

it("keeps successful search result identities and the supplied error value", async () => {
  const { screenSearchResults } = await import("./screenChannel");
  const results = [{ id: "result", agent: "agent" }];
  const error = { message: "Original error value" };
  const reply = screenSearchResults({
    kind: "studio-server-search-results",
    requestId: "request",
    results,
    error,
  });
  expect(reply?.results).toBe(results);
  expect(reply?.error).toBe(error);
});
