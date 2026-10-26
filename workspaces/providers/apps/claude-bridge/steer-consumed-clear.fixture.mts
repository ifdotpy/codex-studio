export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
  resolveSettings,
} from "./steer.fixture.mts";
import { fixtureQuery } from "./steer.fixture.mts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
export const query = (args: FixtureQuery) =>
  fixtureQuery(args, { crashOnClear: true });
