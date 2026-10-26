import {
  createRemovedMessageStore,
  type RemovedMessageUpdate,
} from "../components/removedMessageStorage";
import type { createNativeLocalDrafts } from "./services/localDrafts";

/** Resolve the saved CAS before a later explicit edit can propose other content. */
export function createNativeRemovedMessageStore(
  storage: Storage,
  drafts: Pick<ReturnType<typeof createNativeLocalDrafts>, "updateLocalDraft">,
) {
  const pending = new Map<string, () => Promise<unknown>>();
  let writing: Promise<void> = Promise.resolve();
  const update: RemovedMessageUpdate = <T>(
    key: string,
    fallback: T,
    edit: (current: T) => T,
  ) => {
    const operation = writing.then(async () => {
      const previous = pending.get(key);
      if (previous) {
        await previous();
        if (pending.get(key) === previous) pending.delete(key);
      }
      const run = () => drafts.updateLocalDraft(key, fallback, edit);
      pending.set(key, run);
      const value = await run();
      if (pending.get(key) === run) pending.delete(key);
      return value;
    });
    writing = operation.then(
      () => {},
      () => {},
    );
    return operation;
  };
  return createRemovedMessageStore(storage, update);
}
