import { query as streamingQuery } from "./streaming.fixture.mts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
export {
  resolveSettings,
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./sdk-fixtures.ts";

export function query(input: FixtureQuery) {
  const base = streamingQuery(input);
  return {
    ...base,
    async *[Symbol.asyncIterator]() {
      for await (const frame of base) {
        if (frame.type === "assistant") {
          const { uuid: _uuid, ...withoutUuid } = frame;
          // Legacy provider frames can omit the outer UUID while retaining
          // distinct native message identities inside the assistant message.
          for (const id of ["subagent-first", "subagent-second"])
            yield {
              ...withoutUuid,
              parent_tool_use_id: "parent-tool",
              message: { ...frame.message, id },
            };
          yield { ...frame, error: "billing_error" };
        } else yield frame;
      }
    },
  };
}
