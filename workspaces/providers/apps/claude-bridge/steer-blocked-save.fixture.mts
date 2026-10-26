export { resolveSettings } from "./sdk-fixtures.ts";
export {
  tool,
  createSdkMcpServer,
  forkSession,
  getSessionMessages,
} from "./steer.fixture.mts";
import { fixtureQuery, installSteerFaults } from "./steer.fixture.mts";
import type { FixtureQuery } from "./sdk-fixtures.ts";
installSteerFaults("hold", false);
export const query = (args: FixtureQuery) =>
  fixtureQuery(args, { markInterrupt: true, ignoreInterruptMarker: true });
