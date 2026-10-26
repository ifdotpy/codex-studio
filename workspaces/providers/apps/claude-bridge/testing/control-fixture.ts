import assert from "node:assert/strict";
import type { ControlDependencies } from "../controls.ts";
export const unusedFileControls = {
  rewindFiles: async () => assert.fail("Unexpected file rewind"),
  prepareFileRestore: async () => assert.fail("Unexpected file restore"),
  verifyFileRestore: async () => assert.fail("Unexpected file verification"),
} satisfies Pick<
  ControlDependencies,
  "rewindFiles" | "prepareFileRestore" | "verifyFileRestore"
>;
