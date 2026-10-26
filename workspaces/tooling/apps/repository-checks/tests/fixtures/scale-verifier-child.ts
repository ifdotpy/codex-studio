export interface FixtureMessage {
  stage?: string;
  method?: string;
  path?: string;
  body?: { id: string };
  remaining?: string[];
  deletes?: number;
}

import assert from "node:assert/strict";
import {
  runScaleVerifier,
  type ScaleVerifierLifecycle,
} from "../../../../../client/apps/web/scripts/scaleVerifierTeardown.ts";
const mode = process.argv[2];
const gates = new Map<string, () => void>();
const wait = (stage: string) =>
  new Promise<void>((resolve) => {
    gates.set(stage, resolve);
    process.send?.({ stage });
  });
process.on("message", (message: FixtureMessage) => {
  if (message.stage) gates.get(message.stage)?.();
});
// The lifecycle must keep unresolved work and teardown alive without I/O.
process.channel?.unref();
let interrupted = false;
let remaining = ["lead-1", "worker-1"];
let deletes = 0;
const api = async (method: string, path: string, body?: { id: string }) => {
  process.send?.({ method, path, body });
  if (method === "GET") {
    await wait("audit");
    if (mode === "audit-error") throw new Error("audit failed");
  } else {
    assert.deepEqual(body, { id: "lead-1" });
    deletes += 1;
    await wait("delete");
    if (mode === "lost-delete-reply") throw new Error("delete outcome unknown");
    remaining = [];
  }
};
await runScaleVerifier(
  async ({ setCleanup, setRemaining }: ScaleVerifierLifecycle) => {
    setRemaining(() => ({ agents: remaining, deleteAttempts: deletes }));
    setCleanup(async () => {
      try {
        await api("GET", "/api/transcript/page?id=lead-1");
      } finally {
        await api("POST", "/api/conversation/delete", { id: "lead-1" });
      }
      process.send?.({ stage: "cleaned", remaining, deletes });
    });
    await wait("work");
    if (mode === "error") throw new Error("work failed");
    if (mode === "assertion") assert.fail("work assertion");
    if (mode === "save-error") throw new Error("evidence write failed");
    if (interrupted) throw new Error("interrupted");
  },
  {
    onInterrupt() {
      interrupted = true;
      gates.get("work")?.();
    },
  },
).catch((error: unknown) => {
  process.stderr.write(
    (error instanceof Error ? error.message : String(error)) + "\n",
  );
  process.exitCode ||= 1;
});
process.disconnect?.();
