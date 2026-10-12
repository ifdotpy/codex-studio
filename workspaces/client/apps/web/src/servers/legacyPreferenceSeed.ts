import type { PreferenceFields } from "../sync/uiPreferenceMerge";
import { defaultServerAlias } from "./serverAliases";
import type { StudioServer } from "./registry";

export function legacyPreferenceSeed(
  servers: StudioServer[],
  identities: Map<string, string>,
  existing: PreferenceFields,
  customizedLocalLabel: string | null,
): PreferenceFields {
  const fields: PreferenceFields = {};
  const aliases = new Set<string>();
  for (const server of servers) {
    const identity = identities.get(server.id);
    if (!identity) continue;
    const aliasKey = JSON.stringify([`server-alias:${identity}`]);
    const labelKey = JSON.stringify([`server-label:${identity}`]);
    const derivedAlias = defaultServerAlias(server, aliases);
    aliases.add(server.alias || derivedAlias);
    if (server.alias && server.alias !== derivedAlias && !existing[aliasKey])
      fields[aliasKey] = {
        value: server.alias,
        timestamp: 0,
        writer: "legacy",
      };
    const legacyLabel =
      server.id === "local" ? customizedLocalLabel : server.label;
    if (legacyLabel && !existing[labelKey])
      fields[labelKey] = {
        value: legacyLabel,
        timestamp: 0,
        writer: "legacy",
      };
  }
  const orderKey = JSON.stringify(["server-order"]);
  const order = servers
    .map((server) => identities.get(server.id))
    .filter((identity): identity is string => !!identity);
  if (order.length > 1 && !existing[orderKey])
    fields[orderKey] = { value: order, timestamp: 0, writer: "legacy" };
  return fields;
}
