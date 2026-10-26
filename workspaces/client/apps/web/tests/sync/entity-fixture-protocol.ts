import assert from "node:assert/strict";
import type { ChildProcess } from "node:child_process";
import { waitForFixturePort } from "../fixture-readiness.ts";
import type { SyncPullResponse, entityPullFixture } from "../playwright.ts";
import type {
  ApiGetPath,
  GetResult,
  JsonInputOf,
  PostBody,
} from "../../src/apiTypes.ts";

/** This fixture's stdout protocol is emitted by our own Node child. */
function parseFixtureReply(line: string) {
  const value: unknown = JSON.parse(line);
  if (
    !value ||
    typeof value !== "object" ||
    !("id" in value) ||
    typeof value.id !== "string"
  )
    return;
  // The envelope is checked above; the child owns the remaining JSON fields.
  return value as { id: string; [key: string]: unknown };
}

export function fixtureProtocol(child: ChildProcess) {
  assert.ok(child.stdout);
  assert.ok(child.stderr);
  let log = "",
    output = "";
  type Reply = NonNullable<ReturnType<typeof parseFixtureReply>>;
  const replies = new Map<string, Reply>();
  const waiters = new Map<
    string,
    { resolve: (reply: Reply) => void; timeout: NodeJS.Timeout }
  >();
  child.stderr.on("data", (chunk: unknown) => {
    log += String(chunk);
  });
  const port = waitForFixturePort(child, { diagnostics: () => log });
  child.stdout.on("data", (chunk: unknown) => {
    output += String(chunk);
    const lines = output.split("\n");
    output = lines.pop() || "";
    for (const line of lines) {
      try {
        const reply = parseFixtureReply(line);
        if (!reply) continue;
        const waiter = waiters.get(reply.id);
        if (waiter) {
          clearTimeout(waiter.timeout);
          waiters.delete(reply.id);
          waiter.resolve(reply);
        } else replies.set(reply.id, reply);
      } catch {}
    }
  });
  return {
    port,
    async command(
      id: string,
      params: Record<string, string | number | boolean | null | undefined>,
    ) {
      assert.ok(child.stdin);
      const saved = replies.get(id);
      const pending = saved
        ? Promise.resolve(saved)
        : new Promise<Reply>((resolve, reject) => {
            const timeout = setTimeout(() => {
              waiters.delete(id);
              reject(new Error(`Fixture command ${id} timed out: ${log}`));
            }, 8000);
            waiters.set(id, { resolve, timeout });
          });
      child.stdin.write(
        `${JSON.stringify({ id, method: "fixture/entity-change", params })}\n`,
      );
      const reply = await pending;
      // This legacy hook is no longer implemented by rust-ui-fixture.ts. Keep
      // its old test surface, narrowing rather than inventing a server contract.
      assert.equal(typeof reply.ok, "boolean");
      assert.ok(typeof reply.seq === "number");
      assert.ok(typeof reply.entityId === "string");
      return {
        ...reply,
        ok: reply.ok,
        seq: reply.seq,
        entityId: reply.entityId,
      };
    },
  };
}

/** Generated response boundary for the fixture's HTTP routes. */
export async function fixtureGet<Path extends ApiGetPath>(
  origin: string,
  path: Path,
  query = "",
): Promise<GetResult<Path>> {
  const response = await fetch(new URL(`${path}${query}`, origin));
  assert.equal(response.status, 200, `${path}${query} status`);
  return response.json();
}

export function pullPage(value: ReturnType<typeof entityPullFixture>) {
  assert.ok("documents" in value);
  return value;
}

/** Narrow the removed mutation side channel without declaring a current wire contract. */
export function legacyMutationEntities(value: unknown) {
  const body = fixtureObject(value);
  if (!Array.isArray(body._syncEntities)) return;
  return body._syncEntities.map((item: unknown) => {
    const row = fixtureObject(item);
    assert.ok(
      typeof row.id === "string" &&
        typeof row.seq === "number" &&
        typeof row.collection === "string",
    );
    assert.ok(row.op === "upsert" || row.op === "delete");
    return { id: row.id, seq: row.seq, collection: row.collection, op: row.op };
  });
}

export function legacyMutationDocuments(value: unknown) {
  const body = fixtureObject(value);
  if (!Array.isArray(body._syncEntities)) return;
  return body._syncEntities.map(
    (
      item: unknown,
    ): import("../../src/generated/api.ts").components["schemas"]["SyncDocument"] => {
      const row = fixtureObject(item);
      assert.ok(
        typeof row.id === "string" &&
          typeof row.seq === "number" &&
          typeof row.payload === "string" &&
          typeof row._deleted === "boolean",
      );
      return {
        id: row.id,
        seq: row.seq,
        payload: row.payload,
        _deleted: row._deleted,
      };
    },
  );
}

export function entityRows<
  T extends JsonInputOf<SyncPullResponse>["documents"][number],
>(documents: T[]) {
  const isEntity = (
    document: T,
  ): document is Extract<T, { collection: string }> => "collection" in document;
  return documents.map((document) => {
    assert.ok(isEntity(document));
    return document;
  });
}

export function fixtureObject(value: unknown): Record<string, unknown> {
  assert.ok(value && typeof value === "object" && !Array.isArray(value));
  return value as Record<string, unknown>;
}

/** Both intercepted mutations consume only their generated request ID field. */
export function fixtureRequestId(
  value: unknown,
): PostBody<"/api/answer">["id"] {
  const body = fixtureObject(value);
  assert.ok(typeof body.id === "string");
  return body.id;
}

export function fixtureItem<T>(items: readonly T[], index: number): T {
  const item = items[index];
  assert.ok(item !== undefined, `Fixture item ${index} exists`);
  return item;
}
