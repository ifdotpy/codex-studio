import type { ServerAccessRequest } from "./accessContract";
const prefix = "studio-server-management-v1:";
const legacyKey = "studio-server-discovery-pending-v1";
export class ServerManagementRequests {
  constructor(
    private scope: string,
    private storage: Storage,
    private lock: (name: string, run: () => Promise<void>) => Promise<void>,
    private send: (request: ServerAccessRequest) => Promise<void>,
  ) {}
  private key(id: string) {
    return prefix + encodeURIComponent(this.scope) + ":" + id;
  }
  pending(): ServerAccessRequest[] {
    const records: ServerAccessRequest[] = [];
    const start = this.key("");
    for (let i = 0; i < this.storage.length; i++) {
      const key = this.storage.key(i);
      if (key?.startsWith(start))
        records.push(JSON.parse(this.storage.getItem(key)!));
    }
    const legacy = this.storage.getItem(legacyKey);
    if (legacy) {
      const previous = JSON.parse(legacy) as ServerAccessRequest;
      if (!records.some((value) => value.requestId === previous.requestId))
        records.push(previous);
    }
    return records;
  }
  async run(request: ServerAccessRequest) {
    await this.lock("studio-server-management:" + this.scope, async () => {
      const legacy = this.storage.getItem(legacyKey);
      if (legacy) {
        const previous = JSON.parse(legacy) as ServerAccessRequest;
        this.storage.setItem(this.key(previous.requestId), legacy);
        this.storage.removeItem(legacyKey);
      }
      const pending = this.pending();
      const fingerprint = (value: ServerAccessRequest) =>
        JSON.stringify({ ...value, requestId: "" });
      const same = pending.find(
        (value) => fingerprint(value) === fingerprint(request),
      );
      if (pending.length && !same)
        throw new Error(
          "Retry the saved server request before you start another request.",
        );
      const body = same || request;
      this.storage.setItem(this.key(body.requestId), JSON.stringify(body));
      await this.send(body);
      // Remove only this operation, after its own response proves completion.
      this.storage.removeItem(this.key(body.requestId));
    });
  }
}
