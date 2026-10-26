import type {
  SessionFixtureQuery,
  SessionFixtureMessage,
} from "./fixtures/session-sdk-types.ts";
import { randomUUID } from "node:crypto";
import { inputText, waitForFile } from "./sdk-fixtures.ts";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
  resolveSettings,
} from "./sdk-fixtures.ts";

// Observe visits to the real bridge's known pending-tool map. No synthetic load.
let visits = 0;
const values = Map.prototype.values;
Map.prototype.values = function () {
  if (this.has("watched-tool")) visits++;
  return values.call(this);
};

export function query({ prompt, options }: SessionFixtureQuery) {
  const abort = new AbortController();
  const note = (phase: string) =>
    process.stdout.write(
      JSON.stringify({
        method: "fixture/clockVisits",
        params: { phase, visits, at: Date.now() },
      }) + "\n",
    );
  return {
    supportedModels: async () => [],
    accountInfo: async () => ({
      email: "test@example.test",
      subscriptionType: "Claude Max",
      apiProvider: "firstParty",
    }),
    initializationResult: async () => ({ commands: [] }),
    close: () => abort.abort(),
    interrupt: async () => {
      note("interrupt");
      abort.abort();
    },
    async *[Symbol.asyncIterator](): AsyncGenerator<
      SessionFixtureMessage,
      void
    > {
      const input = await prompt.next();
      if (input.done) return;
      const owner = inputText(input.value) === "tool";
      yield { type: "user", isReplay: true, uuid: input.value.uuid };
      if (owner)
        yield {
          type: "assistant",
          uuid: randomUUID(),
          message: {
            id: "fixture-assistant",
            content: [
              {
                type: "tool_use",
                id: "watched-tool",
                name: "fixture-tool",
                input: {},
              },
            ],
          },
        };
      const phase = owner ? "tool" : "idle";
      note(phase + "-ready");
      await waitForFile(options.cwd + "/.emit-" + phase, {
        signal: abort.signal,
      });
      visits = 0;
      yield { type: "system", subtype: "background_tasks_changed", tasks: [] };
      note(phase + "-event");
      await waitForFile(options.cwd + "/.finish-" + phase, {
        signal: abort.signal,
      });
    },
  };
}
