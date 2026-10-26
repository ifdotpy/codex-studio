import assert from "node:assert/strict";
import type { Locator } from "@playwright/test";
export async function visibleBounds(locator: Locator) {
  const bounds = await locator.boundingBox();
  assert.ok(bounds, "Fixture element must have visible bounds");
  return bounds;
}
