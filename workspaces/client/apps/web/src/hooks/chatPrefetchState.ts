import type { ResourceVersion } from "../sync/resourceEvents";

export function foregroundTranscriptPending(
  foregroundId: string | null,
  foregroundReady: boolean,
) {
  return foregroundId !== null && !foregroundReady;
}

/** Tracks duplicate recovery signals only while a transcript pull is active. */
export class TranscriptRefreshGate {
  private running = new Map<string, ResourceVersion>();
  private retryAfterFailure = new Set<string>();

  start(agentId: string, version?: ResourceVersion) {
    if (version) this.running.set(agentId, version);
    else this.running.delete(agentId);
  }

  shouldSuppress(agentId: string, version?: ResourceVersion) {
    const current = this.running.get(agentId);
    if (
      !version ||
      current?.epoch !== version.epoch ||
      current.revision !== version.revision
    )
      return false;
    this.retryAfterFailure.add(agentId);
    return true;
  }

  historySettled(agentId: string) {
    this.running.delete(agentId);
  }

  finish(agentId: string, succeeded: boolean, aborted: boolean) {
    this.running.delete(agentId);
    return this.retryAfterFailure.delete(agentId) && !succeeded && !aborted;
  }

  forget(agentId: string) {
    this.running.delete(agentId);
    this.retryAfterFailure.delete(agentId);
  }
}
