import migrationContract from "../../../../contracts/native-migration.json";
import {
  nativeServerContext,
  nativeStorageCutover,
  type NativeServerContext,
  type NativeHostStorageProof,
  type NativeHostStorageCompletion,
  type NativeHostStorageVerification,
} from "./client";
import { openNativePreferences } from "./preferenceStore";

type Owner = Readonly<{ id: string }>;
type Options = { owner?: Owner; report(error: unknown): void };
export type NativeReadyOwner = Readonly<{
  context: NativeServerContext;
  storage: Awaited<ReturnType<typeof openNativePreferences>>;
  migration: NativeHostStorageProof;
}>;

function checkedProof(
  value: Partial<NativeHostStorageProof>,
  owner: Owner,
  sourceIdentity: string,
): NativeHostStorageProof {
  if (
    value.serverId !== owner.id ||
    value.sourceIdentity !== sourceIdentity ||
    value.proofVersion !== migrationContract.proofVersion ||
    typeof value.sourceDigest !== "string" ||
    !/^[a-f0-9]{64}$/.test(value.sourceDigest) ||
    typeof value.inventoryDigest !== "string" ||
    !/^[a-f0-9]{64}$/.test(value.inventoryDigest)
  )
    throw new Error("The saved data verification does not match this server.");
  return Object.freeze({
    serverId: owner.id,
    sourceIdentity,
    proofVersion: value.proofVersion,
    sourceDigest: value.sourceDigest,
    inventoryDigest: value.inventoryDigest,
  });
}

function checkedVerification(
  value: NativeHostStorageVerification,
  owner: Owner,
  source: string,
  expected?: NativeHostStorageProof,
) {
  if (value?.verified !== true || value.hostVerified !== true)
    throw new Error("The native host has not verified all saved data.");
  const proof = checkedProof(value, owner, source);
  if (
    expected &&
    (proof.sourceDigest !== expected.sourceDigest ||
      proof.inventoryDigest !== expected.inventoryDigest)
  )
    throw new Error("The saved data changed before its verification finished.");
  return proof;
}

function checkedCompletion(
  value: NativeHostStorageCompletion,
  selected: Owner,
  source: string,
) {
  if (
    !value ||
    value.complete !== true ||
    !Array.isArray(value.owners) ||
    !value.owners.length
  )
    throw new Error("The native host has not completed the data migration.");
  const proof = checkedProof(value, selected, source);
  const owners = new Set<string>();
  let matched = false;
  for (const owner of value.owners) {
    if (
      !owner ||
      typeof owner.serverId !== "string" ||
      !/^[a-zA-Z0-9_-]{1,128}$/.test(owner.serverId) ||
      owners.has(owner.serverId) ||
      owner.complete !== true ||
      typeof owner.workspaceId !== "string" ||
      !/^[a-f0-9]{32}$/.test(owner.workspaceId) ||
      typeof owner.sourceIdentity !== "string" ||
      !/^[a-f0-9]{64}$/.test(owner.sourceIdentity)
    )
      throw new Error("A saved data owner is incomplete or does not match.");
    checkedProof(owner, { id: owner.serverId }, owner.sourceIdentity);
    owners.add(owner.serverId);
    if (owner.serverId === selected.id) {
      if (owner.sourceIdentity !== source)
        throw new Error("The saved data belongs to another workspace.");
      matched = true;
    }
  }
  if (!matched) throw new Error("The saved data inventory omits this server.");
  return proof;
}

/** The host reads and imports legacy data before any App writer can mount. */
export async function nativeBootstrap(options: Options) {
  const owner = options.owner ?? { id: "local" };
  const ready = new Map<string, Promise<NativeReadyOwner>>();
  const migrateServer = (selection: Owner): Promise<NativeReadyOwner> => {
    const selected = Object.freeze({ id: selection.id });
    let pending = ready.get(selected.id);
    if (!pending) {
      pending = (async () => {
        const guard = await nativeStorageCutover(selected);
        // A legacy complete flag cannot replace the private host proof.
        const existing = guard.migrationComplete
          ? checkedVerification(
              await guard.verify(),
              selected,
              guard.sourceIdentity,
            )
          : undefined;
        let migration = existing;
        let imported: NativeHostStorageCompletion | undefined;
        if (!migration) {
          imported = await guard.migrate();
          const proof = checkedCompletion(
            imported,
            selected,
            guard.sourceIdentity,
          );
          migration = checkedVerification(
            await guard.verify(proof),
            selected,
            guard.sourceIdentity,
            proof,
          );
        }
        const context = await nativeServerContext(selected);
        if (
          imported &&
          imported.owners.find((item) => item.serverId === selected.id)
            ?.workspaceId !== context.workspaceId
        )
          throw new Error("The saved data belongs to another workspace.");
        return Object.freeze({
          context,
          storage: await openNativePreferences(context, options.report),
          migration,
        });
      })().catch((error) => {
        ready.delete(selected.id);
        throw error;
      });
      ready.set(selected.id, pending);
    }
    return pending;
  };
  return Object.freeze({ ...(await migrateServer(owner)), migrateServer });
}
