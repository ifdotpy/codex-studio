import { join } from "node:path";
import { reapFixtureProcessGroups } from "./tests/fixture-process-group.ts";
import { completeWorkerMemoryRun } from "./playwright-workers.ts";

export default async function globalTeardown() {
  const scratchRoot = process.env.CODEX_PLAYWRIGHT_SCRATCH_PARENT;
  if (scratchRoot)
    await reapFixtureProcessGroups(join(scratchRoot, "fixture-process-groups"));
  completeWorkerMemoryRun();
}
