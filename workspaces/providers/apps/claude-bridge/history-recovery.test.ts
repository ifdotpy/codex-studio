import { expect, test } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";

for (const serviceFirst of [true, false])
  for (const damage of [
    "absent",
    "all",
    "partial",
    "malformed uuid",
    "unknown record",
    "unknown system",
    "malformed auxiliary",
    "invalid block",
    "torn",
    "truncated",
    "missing",
  ]) {
    test(`closed composite ${serviceFirst ? "service/human" : "human/service"}: ${damage}`, async () => {
      const base = path.join(
        os.homedir(),
        ".cache/codex-studio/pivot1-v5-history",
      );
      await fs.mkdir(base, { recursive: true });
      const bridge = await startBridge(
        new URL("./fixtures/history-recovery-sdk.ts", import.meta.url),
        "recovery-",
        {
          temporaryBase: base,
          env: {
            HISTORY_DAMAGE: damage,
            STUDIO_CLAUDE_STEER_ECHO_GRACE_MS: "10",
            STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "10",
          },
        },
      );
      try {
        const { turn } = await bridge.call("turn/start", {
          threadId: bridge.thread,
          input: [{ type: "text", text: "anchor" }],
        });
        await bridge.note((n) => n.method === "fixture/anchor");
        const input = [
          { type: "text", text: "service", inputOrigin: "studio_event" },
          { type: "text", text: "human", inputOrigin: "human" },
        ];
        if (!serviceFirst) input.reverse();
        await bridge.call("turn/steer", {
          threadId: bridge.thread,
          expectedTurnId: turn.id,
          clientUserMessageId: "exact-input",
          input,
        });
        const done = await bridge.note(
          (n) => n.method === "turn/completed" && n.params.turn.id === turn.id,
        );
        expect(done.params.turn.unappliedSteers).toEqual(
          ["absent", "unknown system"].includes(damage)
            ? ["exact-input"]
            : undefined,
        );
        const thread = (
          await bridge.call("thread/read", {
            threadId: bridge.thread,
            includeTurns: true,
          })
        ).thread;
        const item = thread.turns[0].items.find(
          (item) => item.id === "exact-input",
        );
        expect(item.nativeIds).toHaveLength(2);
        expect(item.deliveryStatus).toBe(
          damage === "all"
            ? "delivered"
            : ["absent", "unknown system"].includes(damage)
              ? "not_applied"
              : "submitted",
        );
        if (!["all", "absent", "unknown system"].includes(damage))
          expect(item.error).toMatch(/unknown/);
      } finally {
        await bridge.close();
      }
    });
  }
