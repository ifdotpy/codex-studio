import { useServerUploads } from "../servers/ServerServicesContext";
import { useRef, useState, type SetStateAction } from "react";
import { errorText } from "../api";
import type { Attachment } from "./ComposerAttachments";

type Attachments = Record<string, Attachment[]>;
export function useAttachmentDrafts(
  key: string,
  notify: (message: string) => void,
) {
  const { attachmentCancelled } = useServerUploads();
  const scopes = useRef(new Map<string, Attachments>());
  const [, render] = useState(0);
  const retained = (stored: Attachments) =>
    Object.fromEntries(
      Object.entries(stored).map(([id, files]) => [
        id,
        files.filter((asset) => !attachmentCancelled(asset.id)),
      ]),
    );
  const current = retained(scopes.current.get(key) ?? {});
  const setAttachments = async (
    change: SetStateAction<Attachments>,
  ): Promise<boolean> => {
    try {
      const previous = retained(scopes.current.get(key) ?? {});
      const next = typeof change === "function" ? change(previous) : change;
      scopes.current.set(key, retained(next));
      render((version) => version + 1);
      return true;
    } catch (error) {
      notify(errorText(error));
      return false;
    }
  };
  return [current, setAttachments] as const;
}
