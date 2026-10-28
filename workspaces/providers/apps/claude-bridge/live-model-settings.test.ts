import { readFile } from "node:fs/promises";
import path from "node:path";
import { expect, test } from "vitest";
import { startBridge } from "./testing/bridge-harness.ts";

for (const mode of ["default", "plan"] as const) {
  test(`a ${mode} session changes model and fast mode on the same query`, async () => {
    // The fixture writes inside its disposable bridge working directory.
    const bridge = await startBridge(
      new URL("./fixtures/idle-compaction-sdk.ts", import.meta.url),
      "live-settings-",
      {
        env: { STUDIO_SETTINGS_TRACE: "settings-trace" },
        thread: { claude: { permissionMode: mode } },
      },
    );
    try {
      for (const [model, serviceTier] of [
        ["claude-opus-5-5", "default"],
        ["claude-sonnet-5-5", "priority"],
      ] as const) {
        const { turn } = await bridge.call("turn/start", {
          threadId: bridge.thread,
          input: [{ type: "text", text: "continue" }],
          model,
          serviceTier,
        });
        await bridge.note(
          (note) =>
            note.method === "turn/completed" && note.params.turn.id === turn.id,
        );
      }
      const trace: unknown[] = (
        await readFile(path.join(bridge.root, "settings-trace"), "utf8")
      )
        .trim()
        .split("\n")
        .map((line) => JSON.parse(line));
      expect(
        trace.filter(
          (entry) =>
            typeof entry === "object" &&
            entry !== null &&
            "query" in entry &&
            !("persist" in entry && entry.persist === false),
        ),
      ).toHaveLength(1);
      expect(trace).toContainEqual({ model: "claude-sonnet-5-5" });
      expect(trace).toContainEqual({ mode });
      expect(trace).toContainEqual(
        expect.objectContaining({
          flags: expect.objectContaining({ fastMode: true }),
        }),
      );
    } finally {
      await bridge.close();
    }
  });
}
