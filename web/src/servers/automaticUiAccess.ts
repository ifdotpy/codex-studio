import { parseInvitation } from "./pairing";
import type { PairInvitation } from "./pairing";
import type { StudioServer } from "./registry";
import type { DiscoveredServer, DiscoverySnapshot } from "./discoveryModel";
export type AutomaticAccessAttempt = {
  localServerId: string;
  serverId: string;
  origin: string;
  generation: string;
  inviteRequestId: string;
  pairRequestId: string;
  invitation?: PairInvitation;
};
export interface AutomaticAccessStore {
  read(key: string): Promise<AutomaticAccessAttempt | null>;
  save(key: string, value: AutomaticAccessAttempt): Promise<void>;
  remove(key: string): Promise<void>;
}
export interface AutomaticAccessAdapter {
  invite(server: DiscoveredServer, requestId: string): Promise<PairInvitation>;
  pair(origin: string, code: string, requestId: string): Promise<StudioServer>;
  existing(): StudioServer[];
  add(server: StudioServer): void;
  excluded(serverId: string): boolean;
  lock?(key: string, run: () => Promise<void>): Promise<void>;
}
export class AutomaticUiAccess {
  private running: Promise<void> | null = null;
  private latest: DiscoverySnapshot | null = null;
  private failures = new Map<
    string,
    { retryAt: number; delay: number; serverId: string; error: string }
  >();
  constructor(
    private store: AutomaticAccessStore,
    private adapter: AutomaticAccessAdapter,
    private now = Date.now,
  ) {}
  reconcile(snapshot: DiscoverySnapshot): Promise<void> {
    this.latest = snapshot;
    if (this.running) return this.running;
    this.running = this.run(snapshot).finally(() => {
      this.running = null;
    });
    return this.running;
  }
  retry(serverId: string) {
    for (const [key, value] of this.failures)
      if (value.serverId === serverId) this.failures.delete(key);
  }
  stop() {
    this.latest = null;
  }
  private eligible(home: string, peer: DiscoveredServer) {
    const latest = this.latest;
    const row = latest?.servers.find((server) => server.id === peer.id);
    return (
      latest?.localServerId === home &&
      row?.origin === peer.origin &&
      row.generation === peer.generation &&
      !this.adapter.excluded(peer.id) &&
      row.paired &&
      row.status !== "revoked"
    );
  }
  private async run(snapshot: DiscoverySnapshot) {
    const errors: string[] = [];
    for (const peer of snapshot.servers) {
      if (!this.eligible(snapshot.localServerId, peer)) continue;
      const key = JSON.stringify([
        snapshot.localServerId,
        peer.id,
        peer.origin,
        peer.generation,
      ]);
      const saved = this.adapter
        .existing()
        .find((server) => server.id === peer.id);
      if (saved) {
        if (saved.origin !== peer.origin)
          errors.push(`${peer.label}: The saved server has another address.`);
        continue;
      }
      const failure = this.failures.get(key);
      if (failure && failure.retryAt > this.now()) {
        errors.push(failure.error);
        continue;
      }
      try {
        const access = async () => {
          if (!this.eligible(snapshot.localServerId, peer)) return;
          const registered = this.adapter
            .existing()
            .find((server) => server.id === peer.id);
          if (registered) {
            if (registered.origin !== peer.origin)
              throw new Error("The saved server has another address.");
            return;
          }
          let attempt = await this.store.read(key);
          if (!attempt) {
            attempt = {
              localServerId: snapshot.localServerId,
              serverId: peer.id,
              origin: peer.origin,
              generation: peer.generation,
              inviteRequestId: crypto.randomUUID(),
              pairRequestId: crypto.randomUUID(),
            };
            await this.store.save(key, attempt);
          }
          if (
            attempt.localServerId !== snapshot.localServerId ||
            attempt.serverId !== peer.id ||
            attempt.origin !== peer.origin ||
            attempt.generation !== peer.generation
          )
            throw new Error(
              "The saved access request belongs to another server.",
            );
          if (!attempt.invitation) {
            const invitation = await this.adapter.invite(
              peer,
              attempt.inviteRequestId,
            );
            const checked = parseInvitation(
              JSON.stringify(invitation),
              peer.origin,
            );
            if (
              checked.serverId !== peer.id ||
              checked.publicKey !== peer.publicKey
            )
              throw new Error("The invitation belongs to another server.");
            attempt = { ...attempt, invitation: checked };
            await this.store.save(key, attempt);
          }
          if (!this.eligible(snapshot.localServerId, peer)) return;
          const server = await this.adapter.pair(
            peer.origin,
            JSON.stringify(attempt.invitation),
            attempt.pairRequestId,
          );
          if (!this.eligible(snapshot.localServerId, peer)) return;
          if (server.id !== peer.id || server.origin !== peer.origin)
            throw new Error("The paired UI belongs to another server.");
          this.adapter.add(server);
          await this.store.remove(key);
          this.failures.delete(key);
        };
        if (this.adapter.lock) await this.adapter.lock(key, access);
        else await access();
      } catch (error) {
        const delay = Math.min(
          (this.failures.get(key)?.delay ?? 15000) * 2,
          300000,
        );
        const message = `${peer.label}: ${error instanceof Error ? error.message : String(error)}`;
        this.failures.set(key, {
          retryAt: this.now() + delay,
          delay,
          serverId: peer.id,
          error: message,
        });
        errors.push(message);
      }
    }
    if (errors.length) throw new Error(errors.join("\n"));
  }
}
