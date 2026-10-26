import assert from "node:assert/strict";
import type { ChildProcess } from "node:child_process";
import { waitForFixturePort } from "../fixture-readiness.ts";
import type { SyncPullResponse, SyncEntity } from "../playwright.ts";
import type { components } from "../../src/generated/api.ts";
import type { ApiGetPath, GetResult, PostBody } from "../../src/apiTypes.ts";

type JsonValue = components["schemas"]["JsonValue"];
/** Indexed view of the legacy fixture documents, including fields no longer emitted. */
export type FixtureObject = { [key: string]: JsonValue | undefined };
export type FixtureEntity = Omit<SyncEntity, "value"> &
  components["schemas"]["SyncDocument"] &
  FixtureObject & { value: FixtureObject };
type FixtureReply = FixtureObject & {
  id: string;
  ok: boolean;
  seq: number;
  entityId: string;
};

/** The same-program child owns this legacy JSON reply. */
function parseFixtureReply(line: string): FixtureReply {
  return JSON.parse(line);
}

export function fixtureProtocol(child: ChildProcess) {
  let log = "",
    output = "";
  type Reply = NonNullable<ReturnType<typeof parseFixtureReply>>;
  const replies = new Map<string, Reply>();
  const waiters = new Map<
    string,
    { resolve: (reply: Reply) => void; timeout: NodeJS.Timeout }
  >();
  child.stderr!.on("data", (chunk: unknown) => {
    log += String(chunk);
  });
  const port = waitForFixturePort(child, { diagnostics: () => log });
  child.stdout!.on("data", (chunk: unknown) => {
    output += String(chunk);
    const lines = output.split("\n");
    output = lines.pop() || "";
    for (const line of lines) {
      try {
        const reply = parseFixtureReply(line);
        const waiter = waiters.get(reply.id);
        if (waiter) {
          clearTimeout(waiter.timeout);
          waiters.delete(reply.id);
          waiter.resolve(reply);
        } else if (reply.id) replies.set(reply.id, reply);
      } catch {}
    }
  });
  return {
    port,
    async command(
      id: string,
      params: Record<string, string | number | boolean | null | undefined>,
    ) {
      const saved = replies.get(id);
      const pending = saved
        ? Promise.resolve(saved)
        : new Promise<Reply>((resolve, reject) => {
            const timeout = setTimeout(() => {
              reject(new Error(log));
            }, 8000);
            waiters.set(id, { resolve, timeout });
          });
      child.stdin!.write(
        `${JSON.stringify({ id, method: "fixture/entity-change", params })}\n`,
      );
      return pending;
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

/** The fixture owns these pull pages; the indexed view preserves legacy inspection. */
export function pullPage(value: unknown) {
  return value as Omit<SyncPullResponse, "documents"> & {
    documents: FixtureEntity[];
  };
}

/** The mutation's removed side channel is deliberately inspected by legacy specs. */
function mutationDocument(value: unknown) {
  return value as { _syncEntities?: FixtureEntity[] };
}

export function legacyMutationEntities(value: unknown) {
  return mutationDocument(value)._syncEntities;
}

export function legacyMutationDocuments(value: unknown) {
  return mutationDocument(value)._syncEntities;
}

export function entityRows(documents: unknown) {
  return documents as FixtureEntity[];
}

/** The legacy fixture owns the JSON object read at this boundary. */
export function fixtureObject(value: unknown): FixtureObject {
  return value as FixtureObject;
}

/** Both intercepted mutations consume only their generated request ID field. */
export function fixtureRequestId(value: unknown) {
  return (value as Pick<PostBody<"/api/answer">, "id">).id;
}
