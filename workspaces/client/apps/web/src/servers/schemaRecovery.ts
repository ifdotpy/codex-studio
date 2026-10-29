import type { StudioServer } from "./registry";

/**
 * A server scope that met another API schema stays latched until its service is recreated.
 * Probe each latched server; a probe that succeeds proves the schema now matches (a real
 * mismatch or an unreachable server throws), so return those ids for recreation.
 */
export async function recoverMatchingServers(
  servers: readonly StudioServer[],
  latched: (id: string) => boolean,
  probe: (server: StudioServer) => Promise<unknown>,
): Promise<string[]> {
  const results = await Promise.all(
    servers
      .filter((server) => latched(server.id))
      .map((server) =>
        probe(server).then(
          () => server.id,
          () => undefined,
        ),
      ),
  );
  return results.filter((id): id is string => id !== undefined);
}
