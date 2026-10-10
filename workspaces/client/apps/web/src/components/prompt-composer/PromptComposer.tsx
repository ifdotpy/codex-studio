import {
  useSyncExternalStore,
  type DragEventHandler,
  type FormEvent,
  type ReactNode,
} from "react";
import { reportPromptComposerRender } from "./renderProbe";

export type DraftSubscription = (
  session: string,
  listener: () => void,
) => () => void;
export type DraftReader = (session: string) => string;
export type DraftWriter = (
  text: string | ((current: string) => string),
  session?: string,
) => void;

export function usePromptDraft(
  session: string,
  getDraft: DraftReader,
  subscribeDraft: DraftSubscription,
) {
  return useSyncExternalStore(
    (notify) => subscribeDraft(session, notify),
    () => getDraft(session),
    () => getDraft(session),
  );
}

/** Owns the live composer subtree so typing does not render App or the transcript. */
export default function PromptComposer(p: {
  session: string;
  getDraft: DraftReader;
  subscribeDraft: DraftSubscription;
  onSubmit: (event: FormEvent<HTMLFormElement>) => void;
  onDragOver?: DragEventHandler<HTMLFormElement>;
  onDragLeave?: DragEventHandler<HTMLFormElement>;
  onDrop?: DragEventHandler<HTMLFormElement>;
  className?: string;
  children: (draft: string) => ReactNode;
}) {
  reportPromptComposerRender("prompt-composer", p.session);
  const draft = usePromptDraft(p.session, p.getDraft, p.subscribeDraft);
  return (
    <form
      id="composer"
      className={p.className}
      onSubmit={p.onSubmit}
      onDragOver={p.onDragOver}
      onDragLeave={p.onDragLeave}
      onDrop={p.onDrop}
    >
      {p.children(draft)}
    </form>
  );
}
