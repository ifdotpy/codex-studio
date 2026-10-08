import { completeWorkerMemoryRun } from "./playwright-workers.mjs";

export default async function globalTeardown() {
  completeWorkerMemoryRun();
}
