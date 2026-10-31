import { outboxRequestKeys, outboxRequests } from "../browserPersistence";
import type { ServerAccessRequest } from "./accessContract";
function readLegacy(raw: string): ServerAccessRequest {
  const value: ServerAccessRequest = JSON.parse(raw);
  if (
    !value ||
    typeof value.requestId !== "string" ||
    !value.requestId ||
    ((value.action === "revoke" || value.action === "unrevoke") &&
      typeof value.clientId !== "string")
  )
    throw new Error(
      "The saved server request is invalid. Resolve it before retrying.",
    );
  return value;
}
export class ServerManagementRequests {
  private scope: string;
  private outbox: ReturnType<typeof outboxRequests>;
  private storage: Storage;
  private lock: (name: string, run: () => Promise<void>) => Promise<void>;
  private send: (request: ServerAccessRequest) => Promise<void>;
  constructor(
    scope: string,
    storage: Storage,
    lock: (name: string, run: () => Promise<void>) => Promise<void>,
    send: (request: ServerAccessRequest) => Promise<void>,
  ) {
    this.scope = scope;
    this.outbox = outboxRequests(storage);
    this.storage = storage;
    this.lock = lock;
    this.send = send;
  }
  private key(id: string) {
    return encodeURIComponent(this.scope) + ":" + id;
  }
  pending(): ServerAccessRequest[] {
    const records: ServerAccessRequest[] = [];
    const start = this.key("");
    for (const key of outboxRequestKeys(
      this.storage,
      "serverManagement",
      start,
    ))
      records.push(JSON.parse(this.outbox.get("serverManagement", key)!));
    const legacy = this.outbox.get("legacyServerManagement", "");
    if (legacy) {
      const previous = readLegacy(legacy);
      if (!records.some((value) => value.requestId === previous.requestId))
        records.push(previous);
    }
    return records;
  }
  async run(request: ServerAccessRequest) {
    await this.lock("studio-server-management:" + this.scope, async () => {
      const legacy = this.outbox.get("legacyServerManagement", "");
      if (legacy) {
        const previous = readLegacy(legacy);
        this.outbox.set(
          "serverManagement",
          this.key(previous.requestId),
          legacy,
        );
        this.outbox.remove("legacyServerManagement", "");
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
      this.outbox.set(
        "serverManagement",
        this.key(body.requestId),
        JSON.stringify(body),
      );
      await this.send(body);
      // Remove only this operation, after its own response proves completion.
      this.outbox.remove("serverManagement", this.key(body.requestId));
    });
  }
}
