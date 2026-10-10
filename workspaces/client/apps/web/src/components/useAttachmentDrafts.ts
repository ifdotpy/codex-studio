import { useEffect, useRef, useState, type SetStateAction } from "react";
import { saved, errorText } from "../api";
import { attachmentCancelled } from "../sync/uploads";
import { updateLocalDraft } from "../sync/localDraft";
import type { Attachment } from "./ComposerAttachments";

type Attachments = Record<string, Attachment[]>;
const retained = (stored: Attachments) =>
  Object.fromEntries(
    Object.entries(stored).map(([id, files]) => [
      id,
      files.filter((asset) => !attachmentCancelled(asset.id)),
    ]),
  );
export function useAttachmentDrafts(
  key: string,
  notify: (message: string) => void,
) {
  const scopes = useRef(new Map<string, Attachments>());
  const [, render] = useState(0);
  if (!scopes.current.has(key))
    scopes.current.set(key, retained(saved<Attachments>(key, {})));
  useEffect(() => {
    const refresh = (event: StorageEvent) => {
      if (
        event.key === key ||
        event.key?.startsWith("studio-upload-cancelled:")
      ) {
        scopes.current.set(key, retained(saved<Attachments>(key, {})));
        render((version) => version + 1);
      }
    };
    window.addEventListener("storage", refresh);
    return () => window.removeEventListener("storage", refresh);
  }, [key]);
  const setAttachments = async (
    change: SetStateAction<Attachments>,
  ): Promise<boolean> => {
    try {
      // Read the latest shared draft only after taking the cross-tab write lock.
      const next = await updateLocalDraft<Attachments>(key, {}, (current) => {
        const value =
          typeof change === "function" ? change(retained(current)) : change;
        return Object.fromEntries(
          Object.entries(retained(value)).map(([id, files]) => [
            id,
            files.map(({ preview: _preview, ...asset }) => asset),
          ]),
        );
      });
      scopes.current.set(key, retained(saved<Attachments>(key, next)));
      render((version) => version + 1);
      return true;
    } catch (error) {
      notify(errorText(error));
      return false;
    }
  };
  return [scopes.current.get(key)!, setAttachments] as const;
}
