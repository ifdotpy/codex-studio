import migrationContract from "../../../../contracts/native-migration.json";
import { queueMarkdown } from "./markdownQueue";
import { NativeCommandError } from "./commandError";

export type NativeInvoke = (
  command: string,
  args: Record<string, unknown>,
) => Promise<unknown>;
export type LegacyNativeHost = {
  core: { invoke: NativeInvoke };
  event?: {
    listen(
      name: string,
      callback: (event: { payload: unknown }) => void,
    ): Promise<() => void>;
  };
};
export interface NativeClientTransport {
  available(): boolean;
  send<T>(method: string, params: unknown, id: string): Promise<T>;
  listen(callback: (value: unknown) => void): Promise<() => void>;
}

// Existing client identity limit, measured in JavaScript string code units.
const REQUEST_ID_LENGTH = 128;
function assertRequestIdentity(id: string) {
  if (!id || id.length > REQUEST_ID_LENGTH)
    throw new Error("Invalid native request identity.");
}
// One document correlation sequence, shared by both transport factories.
let bootstrapSequence = 0;

/** Validate the native invoke response once for both concrete transports. */
export async function invokeClientRequest<T>(
  invoke: NativeInvoke,
  method: string,
  params: unknown,
  id: string,
): Promise<T> {
  assertRequestIdentity(id);
  // Transport errors never cause an automatic retry of a possibly applied command.
  const value = await invoke("client_request", {
    request: { id, method, params },
  });
  if (
    !value ||
    typeof value !== "object" ||
    !("id" in value) ||
    value.id !== id
  )
    throw new Error("The native client returned a mismatched response.");
  const response = value as {
    result?: T;
    error?: {
      message?: string;
      code?: string;
      status?: number;
      details?: unknown;
    };
  };
  if (Object.hasOwn(response, "result") === Object.hasOwn(response, "error"))
    throw new Error("The native client returned an invalid response.");
  if (Object.hasOwn(response, "error"))
    throw new NativeCommandError(response.error ?? {});
  return response.result as T;
}

export type CutoverInventory = Readonly<{
  metadata: readonly { manifestId: string; sourceSha256: string }[];
  bytes: readonly { manifestId: string; receipt: unknown }[];
}>;
export type NativeArchiveSeal = Readonly<{
  id: string;
  owner: string;
  sha256: string;
  serializedSnapshot: string;
}>;
export type NativeHostStorageProof = Readonly<{
  serverId: string;
  sourceIdentity: string;
  proofVersion: number;
  sourceDigest: string;
  inventoryDigest: string;
}>;
export type NativeHostStorageCompletion = NativeHostStorageProof &
  Readonly<{
    complete: boolean;
    owners: readonly (NativeHostStorageProof &
      Readonly<{
        workspaceId: string;
        complete: boolean;
      }>)[];
  }>;
export type NativeHostStorageVerification = Partial<NativeHostStorageProof> &
  Readonly<{
    verified: boolean;
    hostVerified: boolean;
  }>;
export type NativeSubscriptionEvent<T> = {
  value?: T;
  error?: { code?: string; message: string };
};
export type NativeServerContext = Readonly<{
  handle: string;
  serverId: string;
  workspaceId: string;
  origin: string;
  credentialId: string | null;
  request<T>(method: string, params: unknown, exactId?: string): Promise<T>;
  subscribe<T>(
    topic: string,
    params: Record<string, unknown>,
    receive: (event: NativeSubscriptionEvent<T>) => void,
  ): Promise<() => Promise<void>>;
}>;

/** Shared request, Markdown, cutover and context semantics. No transport fallback. */
export function createNativeClient(transport: NativeClientTransport) {
  async function nativeRequest<T>(
    method: string,
    params: unknown,
    id?: string,
    signal?: AbortSignal,
  ): Promise<T> {
    id ??= await nativeIdentity();
    assertRequestIdentity(id);
    if (method === "markdown.parse")
      return queueMarkdown(
        () => sendNativeRequest<T>(method, params, id),
        signal,
      );
    return sendNativeRequest<T>(method, params, id);
  }

  async function sendNativeRequest<T>(
    method: string,
    params: unknown,
    id: string,
  ): Promise<T> {
    assertRequestIdentity(id);
    return transport.send<T>(method, params, id);
  }

  function listenNativeEvents(callback: (value: unknown) => void) {
    return transport.listen(callback);
  }

  /** A transport correlation ID cannot authorize or replay a durable operation. */
  function nativeIdentity(): Promise<string> {
    return sendNativeRequest<string>(
      "system.uuid",
      {},
      `native-bootstrap:${++bootstrapSequence}`,
    ).then((value) => {
      if (
        typeof value !== "string" ||
        !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
          value,
        )
      )
        throw new Error(
          "The native host returned an invalid request identity.",
        );
      return value;
    });
  }

  /** Replay only original native preference commits before copying their owner. */
  async function prepareLegacyNativePreferences(
    owner: Readonly<{ id: string }>,
    writes: readonly unknown[],
  ) {
    return sendNativeRequest<{ requestIds: string[]; sourceSha256: string }>(
      "service.legacy_preferences",
      { serverId: owner.id, writes },
      await nativeIdentity(),
    );
  }

  async function nativeStorageCutover(owner: Readonly<{ id: string }>) {
    const serverId = owner.id;
    const proof = await sendNativeRequest<{
      token: string;
      serverId: string;
      sourceIdentity: string;
      migrationComplete: boolean;
    }>("service.storage_cutover", { serverId }, await nativeIdentity());
    if (
      !proof ||
      proof.serverId !== serverId ||
      typeof proof.token !== "string" ||
      !proof.token ||
      !/^[0-9a-f]{64}$/.test(proof.sourceIdentity)
    )
      throw new Error("Invalid native storage cutover proof.");
    if (typeof proof.migrationComplete !== "boolean")
      throw new Error("Invalid native migration source seal.");
    return Object.freeze({
      serverId,
      sourceIdentity: proof.sourceIdentity,
      migrationComplete: proof.migrationComplete,
      async migrate() {
        return sendNativeRequest<NativeHostStorageCompletion>(
          migrationContract.migrateMethod,
          { serverId, token: proof.token },
          await nativeIdentity(),
        );
      },
      async verify(expected?: NativeHostStorageProof) {
        const result = await sendNativeRequest<NativeHostStorageVerification>(
          migrationContract.verifyMethod,
          {
            serverId,
            token: proof.token,
            ...(expected
              ? {
                  proofVersion: expected.proofVersion,
                  sourceDigest: expected.sourceDigest,
                  inventoryDigest: expected.inventoryDigest,
                }
              : {}),
          },
          await nativeIdentity(),
        );
        if (result?.verified !== true)
          throw new Error(
            "The native storage cutover proof is no longer valid.",
          );
        return result;
      },
    });
  }

  /** Only the server selector crosses bootstrap. The host resolves its authority. */
  async function nativeServerContext(
    owner: Readonly<{ id: string }>,
  ): Promise<NativeServerContext> {
    const serverId = owner.id;
    const scope = await sendNativeRequest<{
      handle: string;
      serverId: string;
      workspaceId: string;
      origin: string;
      credentialId: string | null;
    }>("service.open", { serverId }, await nativeIdentity());
    if (
      !scope ||
      typeof scope.handle !== "string" ||
      !scope.handle ||
      scope.serverId !== serverId ||
      !/^[0-9a-f]{32}$/.test(scope.workspaceId) ||
      typeof scope.origin !== "string" ||
      !scope.origin ||
      !(
        scope.credentialId === null ||
        (typeof scope.credentialId === "string" &&
          scope.credentialId.length > 0)
      )
    )
      throw new Error("The native server context does not match its owner.");
    const request = async <T>(
      method: string,
      params: unknown,
      exactId?: string,
    ): Promise<T> =>
      sendNativeRequest<T>(
        "service.request",
        { handle: scope.handle, method, params },
        exactId ?? (await nativeIdentity()),
      );
    const subscribe = async <T>(
      topic: string,
      params: Record<string, unknown>,
      receive: (event: NativeSubscriptionEvent<T>) => void,
    ) => {
      const subscription = await nativeIdentity();
      let closed = false;
      const unlisten = await listenNativeEvents((input) => {
        if (closed || !input || typeof input !== "object") return;
        const event = input as { event?: string } & NativeSubscriptionEvent<T>;
        if (event.event === `${scope.handle}:${subscription}`) receive(event);
      });
      try {
        await request("view.subscribe", { ...params, topic, subscription });
      } catch (error) {
        closed = true;
        unlisten();
        throw error;
      }
      return async () => {
        if (closed) return;
        closed = true;
        unlisten();
        await request("view.unsubscribe", { subscription });
      };
    };
    return Object.freeze({
      handle: scope.handle,
      serverId: scope.serverId,
      workspaceId: scope.workspaceId,
      origin: scope.origin,
      credentialId: scope.credentialId,
      request,
      subscribe,
    });
  }

  return Object.freeze({
    hasNativeClient: () => transport.available(),
    nativeRequest,
    listenNativeEvents,
    nativeIdentity,
    prepareLegacyNativePreferences,
    nativeStorageCutover,
    nativeServerContext,
  });
}
