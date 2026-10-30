import { describe, expect, it } from "vitest";
import { addLiveDelta, withLiveText, type LiveText } from "./liveText";

const item = "agent:answer";
const run = (...deltas: [number, string][]) =>
  deltas.reduce<ReadonlyMap<string, LiveText>>(
    (live, [offset, text]) =>
      addLiveDelta(live, { itemId: item, offset, text }),
    new Map(),
  );

describe("live text", () => {
  it("shows live text on top of a durable item that lags behind it", () => {
    const live = run([0, "Hel"], [3, "lo"]).get(item);
    expect(withLiveText("", live)).toBe("Hello");
    expect(withLiveText("Hel", live)).toBe("Hello");
  });

  it("is replaced by the durable item when that is at least as long", () => {
    const live = run([0, "Hel"], [3, "lo"]).get(item);
    expect(withLiveText("Hello", live)).toBe("Hello");
    expect(withLiveText("Hello world", live)).toBe("Hello world");
  });

  it("ignores live text that the durable item is too short to join", () => {
    const live = run([5, " world"]).get(item);
    expect(withLiveText("He", live)).toBe("He");
    expect(withLiveText("Hello", live)).toBe("Hello world");
  });

  it("starts a new run after a lost delta", () => {
    const live = run([0, "Hel"], [10, "end"]).get(item);
    expect(live).toEqual({ start: 10, length: 3, text: "end" });
  });

  it("counts Unicode scalars like the server", () => {
    const live = run([0, "a😀"], [2, "b"]).get(item);
    expect(withLiveText("a", live)).toBe("a😀b");
    expect(withLiveText("a😀", live)).toBe("a😀b");
  });

  it("keeps a bounded number of items", () => {
    let live: ReadonlyMap<string, LiveText> = new Map();
    for (let index = 0; index < 20; index++)
      live = addLiveDelta(live, { itemId: `i${index}`, offset: 0, text: "x" });
    expect(live.size).toBe(8);
    expect(live.has("i19")).toBe(true);
  });
});

