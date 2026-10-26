export { resolveSettings } from "./sdk-fixtures.ts";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./steer.fixture.mts";
import { fixtureQuery } from "./steer.fixture.mts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
export const query = (args: FixtureQuery) =>
  fixtureQuery(args, {
    history: "partial",
    echoPulledPrefix: true,
    holdPulledTail: true,
  });
