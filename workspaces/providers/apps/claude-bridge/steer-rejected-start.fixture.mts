export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
  resolveSettings,
} from "./steer.fixture.mts";
import { fixtureQuery, installSteerFaults } from "./steer.fixture.mts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
installSteerFaults("reject-start", false);
export const query = (args: FixtureQuery) => fixtureQuery(args, {});
