export type PromptComposerRenderProbe = (
  component:
    | "app"
    | "sidebar"
    | "conversation"
    | "sidebar-row"
    | "team-row"
    | "prompt-composer"
    | "turn-history",
  id?: string,
) => void;

declare global {
  interface Window {
    /** Optional hook used by isolated browser checks to count real UI renders. */
    __studioPromptComposerRenderProbe?: PromptComposerRenderProbe;
  }
}

export function reportPromptComposerRender(
  component: Parameters<PromptComposerRenderProbe>[0],
  id?: string,
) {
  if (typeof window !== "undefined")
    window.__studioPromptComposerRenderProbe?.(component, id);
}
