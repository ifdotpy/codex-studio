export type PreferenceSyncServer = {
  id: string;
  origin: string;
  credentialId?: string;
};

type ManagedConnection<T extends PreferenceSyncServer> = {
  server: T;
  ownership: string;
  stop: () => void;
  schemaMismatch: boolean;
  mismatchLastSeen?: number;
  latestLastSeen?: number;
};

export function createPreferenceSyncConnectionManager<
  T extends PreferenceSyncServer,
>(connect: (server: T, onSchemaMismatch: () => void) => () => void) {
  const connections = new Map<string, ManagedConnection<T>>();
  const ownership = (server: T) =>
    JSON.stringify([server.id, server.origin, server.credentialId]);
  const start = (server: T, lastSeen?: number) => {
    const entry: ManagedConnection<T> = {
      server,
      ownership: ownership(server),
      stop: () => {},
      schemaMismatch: false,
      latestLastSeen: lastSeen,
    };
    entry.stop = connect(server, () => {
      if (entry.schemaMismatch) return;
      entry.schemaMismatch = true;
      entry.mismatchLastSeen = entry.latestLastSeen;
    });
    connections.set(server.id, entry);
  };
  const stop = (entry: ManagedConnection<T>) => {
    entry.stop();
    connections.delete(entry.server.id);
  };
  return {
    updateServers(
      servers: T[],
      lastSeenByServer: Record<string, number | undefined>,
    ) {
      const byId = new Map(servers.map((server) => [server.id, server]));
      for (const entry of Array.from(connections.values())) {
        const next = byId.get(entry.server.id);
        if (!next || ownership(next) !== entry.ownership) stop(entry);
      }
      for (const server of servers) {
        const entry = connections.get(server.id);
        if (entry) {
          entry.server = server;
          entry.latestLastSeen = lastSeenByServer[server.id];
        } else start(server, lastSeenByServer[server.id]);
      }
    },
    updateLastSeen(lastSeenByServer: Record<string, number | undefined>) {
      for (const entry of Array.from(connections.values())) {
        const latest = lastSeenByServer[entry.server.id];
        entry.latestLastSeen = latest;
        if (
          entry.schemaMismatch &&
          latest !== undefined &&
          (entry.mismatchLastSeen === undefined ||
            latest > entry.mismatchLastSeen)
        ) {
          const server = entry.server;
          stop(entry);
          start(server, latest);
        }
      }
    },
    stop() {
      for (const entry of Array.from(connections.values())) stop(entry);
    },
  };
}
