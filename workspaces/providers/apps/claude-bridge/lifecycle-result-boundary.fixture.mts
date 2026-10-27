import { query as scriptedQuery } from "./usage-events.fixture.mts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
import { waitForFile } from "./testing/file-signals.ts";
import path from "node:path";
export {
  resolveSettings,
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";

function signal(name: string) {
  process.stdout.write(
    JSON.stringify({ method: "fixture/signal", params: { name } }) + "\n",
  );
}
export function query(args: FixtureQuery) {
  const scripted = scriptedQuery(args);
  return {
    ...scripted,
    close() {
      signal("query-close");
      scripted.close();
    },
    async interrupt() {
      signal("interrupt");
      await scripted.interrupt();
    },
    async *[Symbol.asyncIterator]() {
      const iterator = scripted[Symbol.asyncIterator]();
      let results = 0;
      try {
        for (
          let step = await iterator.next();
          !step.done;
          step = await iterator.next()
        ) {
          yield step.value;
          if (step.value.type === "system" && step.value.subtype === "init") {
            if (process.env.STUDIO_FIXTURE_DEFER_RESULT)
              await args.prompt.next();
            if (process.env.STUDIO_FIXTURE_HOLD_RESULT) {
              signal("ready");
              await waitForFile(path.join(args.options.cwd, "release-result"), {
                childIpc: true,
              });
            }
          }
          if (
            process.env.STUDIO_FIXTURE_HOLD_TOOL_RESULT &&
            step.value.type === "assistant" &&
            "message" in step.value &&
            step.value.message?.content?.some(
              (block) => block.type === "tool_use",
            )
          ) {
            signal("tool-ready");
            await waitForFile(path.join(args.options.cwd, "release-result"), {
              childIpc: true,
            });
          }
          if (step.value.type === "result") {
            signal("result-processed-" + ++results);
            if (process.env.STUDIO_FIXTURE_FINITE_RESULT) return;
          }
        }
      } finally {
        if (process.env.STUDIO_FIXTURE_DELAY_RESULT_CLOSE) {
          signal("close-pending");
          await waitForFile(path.join(args.options.cwd, "release-close"), {
            childIpc: true,
          });
        }
        await iterator.return(undefined);
        signal("result-closed");
      }
    },
  };
}
