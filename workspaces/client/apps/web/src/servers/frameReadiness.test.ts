import { expect, it } from "vitest";
import { setServerFrame } from "./frameReadiness";

it("waits for a replacement server frame to announce readiness", () => {
  const frames = new Map<string, object>();
  const readyFrames = new Set(["remote"]);
  const lifecycle = new Map([
    ["remote", { known: true, ready: true, busy: false, activity: false }],
  ]);
  const replacement = {};

  setServerFrame("remote", replacement, frames, readyFrames, lifecycle);

  expect(frames.get("remote")).toBe(replacement);
  expect(readyFrames.has("remote")).toBe(false);
  expect(lifecycle.get("remote")).toMatchObject({ known: false, ready: false });
});
